use std::{convert::Infallible, future::Future, net::SocketAddr};

use http::{uri::Scheme, Request, Response, Uri};
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Server,
};

pub async fn spawn_blackhole_http_server<H, F>(address: SocketAddr, handler: H) -> Uri
where
    H: Fn(Request<Body>) -> F + Clone + Send + 'static,
    F: Future<Output = std::result::Result<Response<Body>, Infallible>> + Send + 'static,
{
    let uri = Uri::builder()
        .scheme(Scheme::HTTP)
        .authority(address.to_string())
        .path_and_query("/")
        .build()
        .expect("URI should always be valid when starting from `SocketAddr`");

    let make_service = make_service_fn(move |_| {
        let handler = handler.clone();
        let service = service_fn(handler);

        async move { Ok::<_, Infallible>(service) }
    });

    let server = Server::bind(&address).serve(make_service);

    tokio::spawn(async move {
        if let Err(e) = server.await {
            eprintln!("blackhole HTTP server error: {}", e);
        }
    });

    uri
}

pub async fn always_200_response(_: Request<Body>) -> Result<Response<Body>, Infallible> {
    Ok(Response::new(Body::empty()))
}
