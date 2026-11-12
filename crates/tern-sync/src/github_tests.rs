use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default, Clone)]
struct FakeKeychain {
    token: Arc<Mutex<Option<String>>>,
    fail: Arc<AtomicBool>,
}

impl Keychain for FakeKeychain {
    fn get(&self) -> Result<String, keyring::Error> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(keyring::Error::PlatformFailure(Box::new(
                std::io::Error::other("keychain failure"),
            )));
        }
        self.token
            .lock()
            .unwrap()
            .clone()
            .ok_or(keyring::Error::NoEntry)
    }

    fn set(&self, token: &str) -> Result<(), keyring::Error> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(keyring::Error::PlatformFailure(Box::new(
                std::io::Error::other("keychain failure"),
            )));
        }
        *self.token.lock().unwrap() = Some(token.to_owned());
        Ok(())
    }

    fn delete(&self) -> Result<(), keyring::Error> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(keyring::Error::PlatformFailure(Box::new(
                std::io::Error::other("keychain failure"),
            )));
        }
        let mut t = self.token.lock().unwrap();
        if t.is_some() {
            *t = None;
            Ok(())
        } else {
            Err(keyring::Error::NoEntry)
        }
    }

    fn clone_box(&self) -> Box<dyn Keychain> {
        Box::new(self.clone())
    }
}

#[derive(Debug, Clone)]
struct FakeHttp {
    get_responses: Arc<Mutex<HashMap<String, Result<Value, String>>>>,
    send_calls: Arc<Mutex<Vec<(Method, String, Value)>>>,
    send_result: Arc<Mutex<Result<(), String>>>,
}

impl Default for FakeHttp {
    fn default() -> Self {
        Self {
            get_responses: Arc::new(Mutex::new(HashMap::new())),
            send_calls: Arc::new(Mutex::new(Vec::new())),
            send_result: Arc::new(Mutex::new(Ok(()))),
        }
    }
}

impl GithubHttp for FakeHttp {
    fn get(&self, token: &str, path: &str) -> Result<Value, GithubError> {
        assert_eq!(token, "test-token");
        if let Some(res) = self.get_responses.lock().unwrap().get(path) {
            match res {
                Ok(val) => Ok(val.clone()),
                Err(err) => Err(GithubError::Http(err.clone())),
            }
        } else {
            Err(GithubError::Http(format!("unexpected GET {path}")))
        }
    }

    fn send(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Value,
    ) -> Result<(), GithubError> {
        assert_eq!(token, "test-token");
        self.send_calls
            .lock()
            .unwrap()
            .push((method, path.to_owned(), body));
        self.send_result
            .lock()
            .unwrap()
            .clone()
            .map_err(GithubError::Http)
    }

    fn clone_box(&self) -> Box<dyn GithubHttp> {
        Box::new(self.clone())
    }
}

#[test]
fn token_from_keychain() {
    let kc = FakeKeychain::default();
    *kc.token.lock().unwrap() = Some("kc-secret".to_owned());
    let _g1 = set_test_keychain(Box::new(kc));

    let gh = FakeGh::default();
    let _g2 = set_test_gh(Box::new(gh.clone()));

    let (t, src) = token().unwrap();
    assert_eq!(t, "kc-secret");
    assert_eq!(src, TokenSource::Keychain);
    assert!(gh.calls.lock().unwrap().is_empty());
}

#[test]
fn token_from_keychain_whitespace_falls_back_to_gh() {
    let kc = FakeKeychain::default();
    *kc.token.lock().unwrap() = Some("   \t  \n".to_owned());
    let _g1 = set_test_keychain(Box::new(kc));

    let gh = FakeGh::default();
    gh.respond(
        "auth token",
        Ok(GhOutput {
            success: true,
            stdout: b"gh-secret\n".to_vec(),
            stderr: Vec::new(),
        }),
    );
    let _g2 = set_test_gh(Box::new(gh));

    let (t, src) = token().unwrap();
    assert_eq!(t, "gh-secret");
    assert_eq!(src, TokenSource::GhCli);
}

#[test]
fn token_from_gh_cli_when_keychain_empty() {
    let kc = FakeKeychain::default();
    let _g1 = set_test_keychain(Box::new(kc));

    let gh = FakeGh::default();
    gh.respond(
        "auth token",
        Ok(GhOutput {
            success: true,
            stdout: b"gh-token-123\n".to_vec(),
            stderr: Vec::new(),
        }),
    );
    let _g2 = set_test_gh(Box::new(gh));

    let (t, src) = token().unwrap();
    assert_eq!(t, "gh-token-123");
    assert_eq!(src, TokenSource::GhCli);
}

#[test]
fn token_no_token_when_both_empty() {
    let kc = FakeKeychain::default();
    let _g1 = set_test_keychain(Box::new(kc));

    let gh = FakeGh::default();
    gh.respond(
        "auth token",
        Ok(GhOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"not logged in".to_vec(),
        }),
    );
    let _g2 = set_test_gh(Box::new(gh));

    assert!(matches!(token().unwrap_err(), GithubError::NoToken));
}

#[test]
fn token_no_token_when_gh_whitespace_only() {
    let kc = FakeKeychain::default();
    let _g1 = set_test_keychain(Box::new(kc));

    let gh = FakeGh::default();
    gh.respond(
        "auth token",
        Ok(GhOutput {
            success: true,
            stdout: b"   \n".to_vec(),
            stderr: Vec::new(),
        }),
    );
    let _g2 = set_test_gh(Box::new(gh));

    assert!(matches!(token().unwrap_err(), GithubError::NoToken));
}

#[test]
fn save_token_stores_trimmed_value() {
    let kc = FakeKeychain::default();
    let _g = set_test_keychain(Box::new(kc.clone()));

    save_token("  my-new-token \n").unwrap();
    assert_eq!(kc.token.lock().unwrap().as_deref(), Some("my-new-token"));
}

#[test]
fn save_token_error_reported() {
    let kc = FakeKeychain::default();
    kc.fail.store(true, Ordering::SeqCst);
    let _g = set_test_keychain(Box::new(kc));

    let err = save_token("token").unwrap_err();
    assert!(matches!(err, GithubError::Keychain(_)));
}

#[test]
fn forget_token_removes_token_and_is_idempotent() {
    let kc = FakeKeychain::default();
    *kc.token.lock().unwrap() = Some("old-token".to_owned());
    let _g = set_test_keychain(Box::new(kc.clone()));

    forget_token().unwrap();
    assert!(kc.token.lock().unwrap().is_none());

    // Second call when already deleted succeeds without error.
    forget_token().unwrap();
}

#[test]
fn forget_token_error_reported() {
    let kc = FakeKeychain::default();
    kc.fail.store(true, Ordering::SeqCst);
    let _g = set_test_keychain(Box::new(kc));

    let err = forget_token().unwrap_err();
    assert!(matches!(err, GithubError::Keychain(_)));
}

#[test]
fn gist_find_matches_exact_description() {
    let http = FakeHttp::default();
    http.get_responses.lock().unwrap().insert(
        "/gists?per_page=100&page=1".into(),
        Ok(json!([
            { "id": "other-gist", "description": "not-ours" },
            { "id": "null-desc", "description": null },
            { "id": "found-id", "description": "tern-sync" }
        ])),
    );
    let _g = set_test_http(Box::new(http));

    let gist = Gist::new("test-token".into());
    assert_eq!(gist.find().unwrap(), Some("found-id".into()));
}

#[test]
fn gist_find_paginates_until_found() {
    let http = FakeHttp::default();
    let page1: Vec<Value> = (0..100)
        .map(|i| json!({ "id": format!("id-{i}"), "description": "other" }))
        .collect();
    http.get_responses
        .lock()
        .unwrap()
        .insert("/gists?per_page=100&page=1".into(), Ok(Value::Array(page1)));
    http.get_responses.lock().unwrap().insert(
        "/gists?per_page=100&page=2".into(),
        Ok(json!([
            { "id": "target-on-page-2", "description": "tern-sync" }
        ])),
    );
    let _g = set_test_http(Box::new(http));

    let gist = Gist::new("test-token".into());
    assert_eq!(gist.find().unwrap(), Some("target-on-page-2".into()));
}

#[test]
fn gist_find_stops_when_page_less_than_100() {
    let http = FakeHttp::default();
    let page1: Vec<Value> = (0..42)
        .map(|i| json!({ "id": format!("id-{i}"), "description": "other" }))
        .collect();
    http.get_responses
        .lock()
        .unwrap()
        .insert("/gists?per_page=100&page=1".into(), Ok(Value::Array(page1)));
    let _g = set_test_http(Box::new(http));

    let gist = Gist::new("test-token".into());
    assert_eq!(gist.find().unwrap(), None);
}

#[test]
fn gist_fetch_returns_none_when_no_gist() {
    let http = FakeHttp::default();
    http.get_responses
        .lock()
        .unwrap()
        .insert("/gists?per_page=100&page=1".into(), Ok(json!([])));
    let _g = set_test_http(Box::new(http));

    let gist = Gist::new("test-token".into());
    assert!(gist.fetch().unwrap().is_none());
}

#[test]
fn gist_fetch_decodes_valid_bundle() {
    let http = FakeHttp::default();
    http.get_responses.lock().unwrap().insert(
        "/gists?per_page=100&page=1".into(),
        Ok(json!([{ "id": "g123", "description": "tern-sync" }])),
    );
    http.get_responses.lock().unwrap().insert(
        "/gists/g123".into(),
        Ok(json!({
            "files": {
                "tern-meta.json": {
                    "content": "{\n  \"version\": 1,\n  \"hash\": \"h789\",\n  \"updated_at\": 1000,\n  \"device\": \"test-mac\"\n}"
                },
                "vault.age.b64": {
                    "content": "dmF1bHQtYnl0ZXM=\n"
                },
                "data.age.b64": {
                    "content": "c2VhbGVkLWJ5dGVz\n"
                }
            }
        })),
    );
    let _g = set_test_http(Box::new(http));

    // Through the sync's Remote seam, which is how a sync reaches the gist.
    let gist = Gist::new("test-token".into());
    let bundle = crate::bundle::Remote::fetch(&gist).unwrap().unwrap();
    assert_eq!(bundle.hash, "h789");
    assert_eq!(bundle.updated_at, 1000);
    assert_eq!(bundle.device, "test-mac");
    assert_eq!(bundle.vault, b"vault-bytes");
    assert_eq!(bundle.sealed, b"sealed-bytes");
}

#[test]
fn gist_fetch_fails_when_file_missing_or_corrupt() {
    let http = FakeHttp::default();
    http.get_responses.lock().unwrap().insert(
        "/gists?per_page=100&page=1".into(),
        Ok(json!([{ "id": "g1", "description": "tern-sync" }])),
    );
    // Missing tern-meta.json
    http.get_responses
        .lock()
        .unwrap()
        .insert("/gists/g1".into(), Ok(json!({ "files": {} })));
    let _g = set_test_http(Box::new(http.clone()));

    let gist = Gist::new("test-token".into());
    let err = gist.fetch().unwrap_err();
    assert!(matches!(err, GithubError::Format(ref msg) if msg.contains("tern-meta.json missing")));

    // Invalid base64 in vault.age.b64
    http.get_responses.lock().unwrap().insert(
        "/gists/g1".into(),
        Ok(json!({
            "files": {
                "tern-meta.json": {
                    "content": "{\"version\": 1, \"hash\": \"h\", \"updated_at\": 0, \"device\": \"d\"}"
                },
                "vault.age.b64": {
                    "content": "not-valid-base64!!!"
                },
                "data.age.b64": {
                    "content": "AAA="
                }
            }
        })),
    );
    let err2 = gist.fetch().unwrap_err();
    assert!(matches!(err2, GithubError::Format(ref msg) if msg.contains("vault.age.b64")));
}

#[test]
fn gist_push_creates_new_gist_when_none_exists() {
    let http = FakeHttp::default();
    http.get_responses
        .lock()
        .unwrap()
        .insert("/gists?per_page=100&page=1".into(), Ok(json!([])));
    let _g = set_test_http(Box::new(http.clone()));

    let gist = Gist::new("test-token".into());
    gist.push("my-hash", 1, "device-x", b"v-bytes", b"s-bytes")
        .unwrap();

    let calls = http.send_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let (method, path, body) = &calls[0];
    assert_eq!(*method, Method::Post);
    assert_eq!(path, "/gists");
    assert_eq!(body["description"], "tern-sync");
    assert_eq!(body["public"], false);

    let files = &body["files"];
    assert_eq!(files[VAULT]["content"], B64.encode(b"v-bytes"));
    assert_eq!(files[DATA]["content"], B64.encode(b"s-bytes"));

    let meta_str = files[META]["content"].as_str().unwrap();
    assert!(meta_str.contains("\"hash\": \"my-hash\""));
    assert!(meta_str.contains("\"device\": \"device-x\""));
}

#[test]
fn gist_push_patches_existing_gist() {
    let http = FakeHttp::default();
    http.get_responses.lock().unwrap().insert(
        "/gists?per_page=100&page=1".into(),
        Ok(json!([{ "id": "existing-456", "description": "tern-sync" }])),
    );
    let _g = set_test_http(Box::new(http.clone()));

    let gist = Gist::new("test-token".into());
    crate::bundle::Remote::push(&gist, "hash-patched", 2, "device-y", b"v2", b"s2").unwrap();

    let calls = http.send_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let (method, path, body) = &calls[0];
    assert_eq!(*method, Method::Patch);
    assert_eq!(path, "/gists/existing-456");
    assert!(body.get("description").is_none());
    assert!(body["files"].is_object());
}

#[test]
fn unconfigured_seams_in_test_return_error() {
    let kc = current_keychain();
    assert!(kc.get().is_err());
    assert!(kc.set("x").is_err());
    assert!(kc.delete().is_err());

    let gh = current_gh();
    assert!(gh.run(&["auth", "token"]).is_err());

    let http = current_http();
    assert!(http.get("tok", "/test").is_err());
    assert!(http.send("tok", Method::Post, "/test", json!({})).is_err());
    assert!(http.send("tok", Method::Patch, "/test", json!({})).is_err());
}

#[test]
fn debug_never_shows_the_token() {
    let g = Gist::new("ghp_secret123".to_owned());
    assert!(!format!("{g:?}").contains("ghp_secret123"));
    assert_eq!(format!("{g:?}"), "Gist { .. }");
}

/// Serves `body` once on localhost and returns the response a plain ureq agent gets for it.
fn served(body: String) -> ureq::http::Response<ureq::Body> {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request);
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
    });
    ureq::get(format!("http://127.0.0.1:{port}/"))
        .call()
        .unwrap()
}

#[test]
fn a_response_at_the_cap_is_refused_and_one_byte_under_is_parsed() {
    // ureq reads once more after `cap` bytes to find the end, so a body of exactly `cap`
    // bytes already trips its limit: the largest accepted body is `cap - 1`.
    let cap = 64;
    // {"k":"<pad>"} is 8 bytes plus the padding.
    let json = |len: usize| format!(r#"{{"k":"{}"}}"#, "a".repeat(len - 8));
    let mut under = served(json(cap - 1));
    let value = read_json_capped(under.body_mut(), cap as u64).unwrap();
    assert_eq!(value["k"].as_str().unwrap().len(), cap - 9);
    let mut at = served(json(cap));
    assert!(read_json_capped(at.body_mut(), cap as u64).is_err());
}

/// The limit is part of the behaviour: a slipped `*` in the arithmetic is a test failure.
#[test]
fn the_gist_response_limit_is_four_mebibytes() {
    assert_eq!(MAX_RESPONSE, 4 * 1024 * 1024);
}

#[test]
fn every_method_maps_to_its_http_verb() {
    assert_eq!(verb(Method::Post), "POST");
    assert_eq!(verb(Method::Patch), "PATCH");
}

/// `send_json` put a JSON Content-Type on the request; the hand-built one must keep doing so.
#[test]
fn a_write_request_carries_the_verb_the_token_and_a_json_content_type() {
    let body = serde_json::json!({"files": {}});
    for (method, verb) in [(Method::Post, "POST"), (Method::Patch, "PATCH")] {
        let r = write_request("tok", method, "https://api.github.com/gists", &body).unwrap();
        assert_eq!(r.method().as_str(), verb);
        let h = |name: &str| r.headers().get(name).and_then(|v| v.to_str().ok());
        assert_eq!(h("authorization"), Some("Bearer tok"));
        assert_eq!(h("accept"), Some("application/vnd.github+json"));
        assert_eq!(h("user-agent"), Some("tern"));
        assert_eq!(h("content-type"), Some("application/json; charset=utf-8"));
    }
}
