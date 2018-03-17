extern crate futures;
extern crate http;
extern crate tower;
extern crate tower_grpc;
extern crate tower_h2;

struct Options {
    allowed_request_headers: Option<Vec<http::header::HeaderName>>,
    cors_for_registered_endpoints_only: bool,
    origin_filter: Option<Box<Fn(&str) -> bool>>,
}

/// A builder for a `Server` instance.
///
/// Use `Server::builder()` to create a new `ServerBuilder`.
pub struct ServerBuilder(Options);

/// A server that wraps a gRPC `tower::Service`, and adds transparent proxying of gRPC-Web requests.
///
/// Ordinary gRPC requests will be passed through transparently, while gRPC-Web and CORS requests
/// will be intercepted and converted into ordinary gRPC requests.
pub struct Server<S>(S, Options);

/// A future that is the result of a `Server` call.
///
/// For a gRPC request, the future yields a gRPC response.  For gRPC-Web requests or CORS requests,
/// the future yields a matching gRPC-Web response or CORS response.
pub struct ServerFuture<F>(InnerServerFuture<F>);

enum InnerServerFuture<F> {
    GrpcWeb(F),
    Grpc(F),
}

/// A client that wraps a transport `tower::Service`, and adds conversion of gRPC requests into
/// gRPC-Web requests.
pub struct Client<S>(S);

/// A future that is the result of a `Client` call.
///
/// The incoming gRPC-Web response will be converted back into an ordinary gRPC response.
pub struct ClientFuture<F>(F);

impl ServerBuilder {
    /// Creates a new builder for building `Server` instances.
    pub fn new() -> ServerBuilder {
        ServerBuilder(Options {
            allowed_request_headers: None,
            cors_for_registered_endpoints_only: true,
            origin_filter: None,
        })
    }

    /// Allows for customizing what CORS Origin requests are allowed.
    ///
    /// This is controlling the CORS pre-flight `Access-Control-Allow-Origin`. This mechanism allows
    /// you to limit the availability of the APIs based on the domain name of the calling website
    /// (Origin). You can provide a function that filters the allowed Origin values.
    ///
    /// The default behaviour is `*`, i.e. to allow all calling websites.
    ///
    /// The relevant CORS pre-flight docs:
    /// https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Access-Control-Allow-Origin
    pub fn origin_filter(mut self, value: Box<Fn(&str) -> bool>) -> Self {
        self.0.origin_filter = Some(value);
        self
    }

    /// Allows for customizing whether OPTIONS requests with the `X-GRPC-WEB` header will only be
    /// accepted if they match a registered gRPC endpoint.
    ///
    /// This should be set to false to allow handling gRPC requests for unknown endpoints (e.g. for
    /// proxying).
    ///
    /// The default behaviour is `true`, i.e. only allows CORS requests for registered endpoints.
    pub fn cors_for_registered_endpoints_only(mut self, value: bool) -> Self {
        self.0.cors_for_registered_endpoints_only = value;
        self
    }

    /// Allows for customizing what gRPC request headers a browser can add.
    ///
    /// This is controlling the CORS pre-flight `Access-Control-Allow-Headers` method and applies to
    /// *all* gRPC handlers. However, a special `*` value can be passed in that allows the browser
    /// client to provide *any* header, by explicitly whitelisting all
    /// `Access-Control-Request-Headers` of the pre-flight request.
    ///
    /// The default behaviour is `*`, allowing all browser client headers. This option overrides
    /// that default, while maintaining a whitelist for gRPC-internal headers.
    ///
    /// Unfortunately, since the CORS pre-flight happens independently from gRPC handler execution,
    /// it is impossible to automatically discover it from the gRPC handler itself.
    ///
    /// The relevant CORS pre-flight docs:
    /// https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Access-Control-Allow-Headers
    pub fn allowed_request_headers<V>(mut self, value: V) -> Self
    where
        V: Into<Vec<http::header::HeaderName>>,
    {
        self.0.allowed_request_headers = Some(value.into());
        self
    }

    /// Builds a `Server` out of this `ServerBuilder`, given the specified `tower::Service` to use
    /// as the backing gRPC service.
    pub fn build<S>(self, service: S) -> Server<S> {
        Server(service, self.0)
    }
}

impl<S> Server<S> {
    /// Constructs a new `Server` using the default options.
    ///
    /// Use `ServerBuilder::new()` if you want to customize these options.
    fn new(service: S) -> Server<S> {
        ServerBuilder::new().build(service)
    }
}

impl<S, B1, B2> tower::Service for Server<S>
where
    S: tower::Service<Request = http::Request<B1>, Response = http::Response<B2>>,
{
    type Request = http::Request<B1>;
    type Response = http::Response<B2>;
    type Error = S::Error;
    type Future = ServerFuture<S::Future>;

    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        if is_grpc_web_request(&req) {
            let result = self.0.call(request_from_web_to_grpc(req));
            ServerFuture(InnerServerFuture::GrpcWeb(result))
        } else {
            let result = self.0.call(req);
            ServerFuture(InnerServerFuture::Grpc(result))
        }
    }
}

impl<F, B> futures::Future for ServerFuture<F>
where
    F: futures::Future<Item = http::Response<B>>,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        match self.0 {
            InnerServerFuture::GrpcWeb(ref mut f) => match f.poll() {
                Ok(futures::Async::Ready(r)) => {
                    Ok(futures::Async::Ready(response_from_grpc_to_web(r)))
                }
                other => other,
            },
            InnerServerFuture::Grpc(ref mut f) => f.poll(),
        }
    }
}

impl<S, B1, B2> tower::Service for Client<S>
where
    S: tower::Service<Request = http::Request<B1>, Response = http::Response<B2>>,
{
    type Request = http::Request<B1>;
    type Response = http::Response<B2>;
    type Error = S::Error;
    type Future = ClientFuture<S::Future>;

    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        ClientFuture(self.0.call(request_from_grpc_to_web(req)))
    }
}

impl<F, B> futures::Future for ClientFuture<F>
where
    F: futures::Future<Item = http::Response<B>>,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        match self.0.poll() {
            Ok(futures::Async::Ready(r)) => Ok(futures::Async::Ready(response_from_web_to_grpc(r))),
            other => other,
        }
    }
}

fn is_grpc_web_request<B>(request: &http::Request<B>) -> bool {
    return *request.method() == http::Method::POST
        && request
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.starts_with("application/grpc-web"))
            .unwrap_or(false);
}

fn request_from_grpc_to_web<B>(request: http::Request<B>) -> http::Request<B> {
    unimplemented!()
}

fn response_from_grpc_to_web<B>(response: http::Response<B>) -> http::Response<B> {

}

fn request_from_web_to_grpc<B>(mut request: http::Request<B>) -> http::Request<B> {
    *request.version_mut() = http::version::Version::HTTP_2;
    match request
        .headers_mut()
        .entry(http::header::CONTENT_TYPE)
        .unwrap()
    {
        http::header::Entry::Occupied(mut entry) => {
            let new_value = entry
                .get()
                .to_str()
                .unwrap()
                .replace("application/grpc-web", "application/grpc");
            entry.insert(http::header::HeaderValue::from_str(&new_value).unwrap());
        }
        http::header::Entry::Vacant(entry) => {
            entry.insert(http::header::HeaderValue::from_static("application/grpc"));
        }
    }
    request
}

fn response_from_web_to_grpc<B>(request: http::Response<B>) -> http::Response<B> {
    unimplemented!()
}

#[cfg(test)]
mod test {
    use http;
    use tower;
    use tower_h2;

    fn assert_http_service<S, B1, B2>(service: S)
    where
        S: tower::Service<Request = http::Request<B1>, Response = http::Response<B2>>,
    {
        tower_h2::HttpService::poll_ready(&mut super::Server(service)).unwrap();
    }
}
