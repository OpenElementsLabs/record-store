use http_body_util::BodyExt;

use super::*;

#[test]
fn incoming_request_ids_are_strictly_validated() {
    assert!(RequestId::accept("trace-123.example").is_some());
    assert!(RequestId::accept("").is_none());
    assert!(RequestId::accept("contains a space").is_none());
    assert!(RequestId::accept(&"a".repeat(129)).is_none());
}

#[tokio::test]
async fn errors_use_the_stable_json_envelope() {
    let response = ApiError::not_found(RequestId("request-1".into())).into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(value["error"]["code"], "ROUTE_NOT_FOUND");
    assert_eq!(value["error"]["request_id"], "request-1");
}
