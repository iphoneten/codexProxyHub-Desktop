use axum::{
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
};

pub(super) async fn index() -> Response {
    static_response(
        "text/html; charset=utf-8",
        include_str!("../../web/user/index.html"),
    )
}

pub(super) async fn css() -> Response {
    static_response(
        "text/css; charset=utf-8",
        include_str!("../../web/user/app.css"),
    )
}

pub(super) async fn javascript() -> Response {
    static_response(
        "text/javascript; charset=utf-8",
        include_str!("../../web/user/app.js"),
    )
}

pub(super) async fn admin_index() -> Response {
    static_response(
        "text/html; charset=utf-8",
        include_str!("../../web/admin/index.html"),
    )
}

pub(super) async fn admin_css() -> Response {
    static_response(
        "text/css; charset=utf-8",
        include_str!("../../web/admin/app.css"),
    )
}

pub(super) async fn admin_javascript() -> Response {
    static_response(
        "text/javascript; charset=utf-8",
        include_str!("../../web/admin/app.js"),
    )
}

fn static_response(content_type: &'static str, body: &'static str) -> Response {
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}
