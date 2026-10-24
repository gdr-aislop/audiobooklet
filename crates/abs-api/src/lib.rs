//! Typed async client for the Audiobookshelf REST API, generated at build time by
//! [`progenitor`] from the vendored OpenAPI spec at
//! `third_party/audiobookshelf-openapi/openapi.json`. See `docs/api/README.md` for the
//! rationale and update procedure.
//!
//! Note: the vendored spec does not cover `/api/items/*`, `/api/me/*`, or authentication —
//! see `third_party/audiobookshelf-openapi/README.md`'s "Known gaps" section. Calls against
//! those endpoints are out of scope for this generated client until upstream documents them.

// The generated code carries an `#[allow(elided_named_lifetimes)]`, a lint name newer
// toolchains renamed to `mismatched_lifetime_syntaxes` — allow both here (and the rename
// warning itself) rather than post-processing codegen output in build.rs. A wrapping module is
// required: an `allow` attribute directly on `include!` doesn't reach the expanded items.
#[allow(renamed_and_removed_lints, elided_named_lifetimes, mismatched_lifetime_syntaxes)]
mod client {
    include!(concat!(env!("OUT_DIR"), "/client.rs"));
}
pub use client::*;

mod ext;
pub use ext::{
    AudioFileRef, ChapterRef, ConnectionBuildError, ConnectionOptions, CoverBytes, DeviceInfo, ItemPlaybackInfo, default_user_agent,
    LibraryItemSummary, LibraryItemsError, LibrarySeriesSummary, LocalSession, LocalSessionResult, LocalSessionsSync, LoginError,
    LoginResult, Whoami, SeriesBookSummary, server_date,
    ServerProgress, TrackFileError, TrackFileResponse, error_chain, is_no_answer, is_unreachable_error, is_valid_header_name,
    is_valid_header_value,
    transport_error_kind, validate_client_cert_file,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated client type must exist and be constructible from a base URL — this is
    /// the minimal smoke test that codegen actually produced a usable client.
    #[test]
    fn client_constructs_from_base_url() {
        let _client = Client::new("http://localhost:3000");
    }

    /// Every operation the vendored spec defines should have produced a method with the same
    /// name as its `operationId` and the parameter shape its path/query params imply. Spot-check
    /// a representative handful across different areas of the spec's actual coverage (libraries,
    /// authors) rather than all 46, so this test stays readable while still catching a broken or
    /// incomplete codegen run. This only needs to type-check, never run.
    #[allow(dead_code, clippy::let_underscore_future)]
    fn representative_operations_type_check(client: &Client) {
        let _ = client.get_libraries();
        let _ = client.get_library_by_id(&"id".parse().unwrap(), None, None);
        let _ = client.get_library_items(
            &"id".parse().unwrap(),
            None, None, None, None, None, None, None, None,
        );
        let _ = client.get_library_authors(&"id".parse().unwrap());
        let _ = client.get_author_by_id(&"id".parse().unwrap(), None);
    }

    /// Exercise a real HTTP round-trip against a mock server to prove the generated client
    /// actually sends requests to the right path and deserializes a real response body, not
    /// just that it type-checks.
    #[tokio::test]
    async fn get_libraries_hits_expected_path_and_parses_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraries": []
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let response = client
            .get_libraries()
            .await
            .expect("request should succeed against the mock server")
            .into_inner();

        assert!(response.libraries.is_empty());
    }

    #[tokio::test]
    async fn get_libraries_propagates_server_errors() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let result = client.get_libraries().await;

        assert!(result.is_err(), "a 500 response should surface as an error");
    }

    /// Confirms path-parameter encoding actually substitutes the id into the URL, and that a
    /// non-empty response body round-trips into the typed `Library` struct.
    #[tokio::test]
    async fn get_library_by_id_encodes_id_in_path() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Real Audiobookshelf library IDs are Sequelize-generated UUIDv4s (confirmed against
        // the server's model definition), matching the spec's `format: uuid` on libraryId.
        let library_id = "e4bb1afb-4a4f-4dd6-8be0-e615d233185b";

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/api/libraries/{library_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": library_id,
                "name": "My Library",
                "folders": [],
                "displayOrder": 1,
                "icon": "database",
                "mediaType": "book",
                "provider": "audible",
                "settings": {},
                "createdAt": 0,
                "lastUpdate": 0,
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let library = client
            .get_library_by_id(&library_id.parse().unwrap(), None, None)
            .await
            .expect("request should succeed")
            .into_inner();

        // Every field in the `library` schema is optional (the spec marks none `required`),
        // so typify generates `Option<T>` throughout.
        assert_eq!(
            library.id.expect("id present in response").to_string(),
            library_id
        );
        assert_eq!(
            library.name.expect("name present in response").to_string(),
            "My Library"
        );
    }
}
