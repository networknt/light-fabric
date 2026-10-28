//! Native fixture signing/owned JWKS transport only; verification stays in production.
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    extract::{Request, State},
    response::IntoResponse,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rsa::{
    RsaPrivateKey,
    pkcs1::EncodeRsaPrivateKey,
    rand_core::{OsRng, RngCore},
    traits::PublicKeyParts,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, task::JoinHandle};

struct Key {
    signer: EncodingKey,
    public: Value,
    alg: String,
}
struct Jwks {
    published: Mutex<Value>,
    routes: BTreeMap<String, Value>,
    calls: Mutex<Vec<Value>>,
    delay: u64,
}
pub(crate) struct Tokens {
    keys: BTreeMap<String, Key>,
    recipes: BTreeMap<String, Value>,
    values: BTreeMap<String, String>,
    signing_epochs: BTreeMap<String, i64>,
    state: Arc<Jwks>,
    task: Option<JoinHandle<()>>,
    pub url: Option<String>,
}
impl Drop for Tokens {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Tokens {
    pub async fn start(f: &Value) -> Result<Self> {
        let mut keys = BTreeMap::new();
        for (id, decl) in f["keys"].as_object().into_iter().flatten() {
            let key = RsaPrivateKey::new(&mut OsRng, 2048)?;
            let signer = EncodingKey::from_rsa_der(key.to_pkcs1_der()?.as_bytes());
            let public = json!({"kty":"RSA","kid":id,"use":"sig","n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())});
            keys.insert(
                id.clone(),
                Key {
                    signer,
                    public,
                    alg: decl["alg"].as_str().context("key algorithm")?.into(),
                },
            );
        }
        let public = |ids: &Value| -> Result<Value> {
            Ok(
                json!({"keys":ids.as_array().context("published key IDs")?.iter().map(|id| keys.get(id.as_str().context("key ID")?).map(|k|k.public.clone()).context("unknown published key")).collect::<Result<Vec<_>>>()?}),
            )
        };
        let enabled = f.pointer("/dependencies/jwks").is_some();
        let initial = if enabled {
            public(&f["dependencies"]["jwks"]["publishedKeys"])?
        } else {
            json!({"keys":[]})
        };
        let mut routes = BTreeMap::new();
        for (path, ids) in f["dependencies"]["jwks"]["routes"]
            .as_object()
            .into_iter()
            .flatten()
        {
            routes.insert(path.clone(), public(ids)?);
        }
        let state = Arc::new(Jwks {
            published: Mutex::new(initial),
            routes,
            calls: Mutex::new(vec![]),
            delay: f["dependencies"]["jwks"]["delayMs"].as_u64().unwrap_or(0),
        });
        let (url, task) = if enabled {
            async fn serve(State(s): State<Arc<Jwks>>, r: Request) -> axum::response::Response {
                s.calls.lock().unwrap().push(
                    json!({"target":"jwks","method":r.method().as_str(),"uri":r.uri().to_string()}),
                );
                let reply = if r.uri().path() == "/jwks" {
                    Some(s.published.lock().unwrap().clone())
                } else {
                    s.routes.get(r.uri().path()).cloned()
                };
                if s.delay > 0 {
                    tokio::time::sleep(Duration::from_millis(s.delay)).await;
                }
                match reply {
                    Some(v) => axum::Json(v).into_response(),
                    None => http::StatusCode::NOT_FOUND.into_response(),
                }
            }
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let url = format!("http://{}", listener.local_addr()?);
            let router = Router::new().fallback(serve).with_state(state.clone());
            (
                Some(url),
                Some(tokio::spawn(async move {
                    let _ = axum::serve(listener, router).await;
                })),
            )
        } else {
            (None, None)
        };
        let recipes = f["tokens"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect();
        let mut tokens = Self {
            keys,
            recipes,
            values: BTreeMap::new(),
            signing_epochs: BTreeMap::new(),
            state,
            task,
            url,
        };
        tokens.sign(None)?;
        Ok(tokens)
    }
    pub fn sign(&mut self, step: Option<usize>) -> Result<()> {
        for (name, r) in &self.recipes {
            if r["signAtStep"].as_u64().map(|n| n as usize) != step {
                continue;
            }
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            let mut claims = r["claims"].clone();
            ensure!(
                ["exp", "iat", "nbf"]
                    .iter()
                    .all(|k| claims.get(k).is_none()),
                "absolute token time forbidden"
            );
            claims["iat"] = json!(now + r["issuedOffsetSec"].as_i64().context("issued offset")?);
            claims["exp"] = json!(now + r["expOffsetSec"].as_i64().context("expiry offset")?);
            if let Some(offset) = r["nbfOffsetSec"].as_i64().filter(|n| *n != 0) {
                claims["nbf"] = json!(now + offset);
            }
            let kid = r["kid"].as_str().context("token kid")?;
            let key = self
                .keys
                .get(r["signingKey"].as_str().unwrap_or(kid))
                .context("signing key")?;
            let alg = match r["alg"].as_str().unwrap_or(&key.alg) {
                "RS256" => Algorithm::RS256,
                "RS512" => Algorithm::RS512,
                "HS256" => Algorithm::HS256,
                _ => anyhow::bail!("unsupported token algorithm"),
            };
            let mut h = Header::new(alg);
            h.kid = Some(kid.into());
            let mut secret = [0u8; 32];
            OsRng.fill_bytes(&mut secret);
            let hmac = EncodingKey::from_secret(&secret);
            let signer = if r["signingKind"] == "hmac" {
                &hmac
            } else {
                &key.signer
            };
            self.values
                .insert(name.clone(), encode(&h, &claims, signer)?);
            self.signing_epochs.insert(name.clone(), now);
        }
        Ok(())
    }
    pub fn signing_epochs(&self) -> Value {
        json!(self.signing_epochs)
    }
    pub fn publish(&self, ids: &Value) -> Result<()> {
        let keys = ids
            .as_array()
            .context("JWKS publication")?
            .iter()
            .map(|id| {
                self.keys
                    .get(id.as_str().context("published ID")?)
                    .map(|k| k.public.clone())
                    .context("unknown published key")
            })
            .collect::<Result<Vec<_>>>()?;
        *self.state.published.lock().unwrap() = json!({"keys":keys});
        Ok(())
    }
    pub fn calls(&self) -> Vec<Value> {
        self.state.calls.lock().unwrap().clone()
    }
    pub fn resolve(&self, v: &mut Value) {
        self.transform(v, false);
    }
    pub fn redact(&self, v: &mut Value) {
        self.transform(v, true);
    }
    fn transform(&self, v: &mut Value, redact: bool) {
        match v {
            Value::Object(m) => m.values_mut().for_each(|v| self.transform(v, redact)),
            Value::Array(a) => a.iter_mut().for_each(|v| self.transform(v, redact)),
            Value::String(s) => {
                if !redact && let Some(url) = &self.url {
                    *s = s.replace("${dependency:jwks}", url);
                }
                for (name, token) in &self.values {
                    let placeholder = format!("${{token:{name}}}");
                    *s = if redact {
                        s.replace(token, &placeholder)
                    } else {
                        s.replace(&placeholder, token)
                    };
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn gaps(f: &Value) -> Vec<String> {
    let mut gaps = vec![];
    for r in f["tokens"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(_, v)| v)
    {
        if r["alg"]
            .as_str()
            .is_some_and(|a| !["RS256", "RS512", "HS256"].contains(&a))
        {
            gaps.push("unsupported token algorithm recipe".into());
        }
    }
    for (key, value) in f["dependencies"]["jwks"].as_object().into_iter().flatten() {
        if !["publishedKeys", "routes", "delayMs"].contains(&key.as_str())
            || (key == "delayMs" && value.as_u64().is_none())
        {
            gaps.push(format!("JWKS dependency recipe:{key}"));
        }
    }
    for (key, approved) in [
        (
            "loopbackOauthPrerequisite",
            "synthetic-client-token-cache-v1",
        ),
        ("jwtCachePrerequisite", "caffeine-jwt-jwk-v1"),
        ("loopbackJwksPrerequisite", "synthetic-jks-client-v1"),
    ] {
        if f.get(key).is_some_and(|v| v != approved) {
            gaps.push(format!("unsupported prerequisite:{key}"));
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn jwks_records_declared_and_unexpected_routes_without_hiding_fetches() {
        let t=Tokens::start(&json!({"keys":{},"tokens":{},"dependencies":{"jwks":{"publishedKeys":[],"routes":{"/other":[]}}}})).await.unwrap();
        for (uri, status) in [("/jwks", "200"), ("/other", "200"), ("/undeclared", "404")] {
            let host = t.url.as_ref().unwrap().strip_prefix("http://").unwrap();
            let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
            stream
                .write_all(
                    format!("GET {uri} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            assert!(response.lines().next().unwrap().contains(status));
        }
        assert_eq!(t.calls().len(), 3);
        assert_eq!(t.calls()[2]["uri"], "/undeclared");
    }
    #[tokio::test]
    async fn step_signing_and_redaction_do_not_resign_warm_tokens() {
        let mut t=Tokens::start(&json!({"keys":{"k1":{"alg":"RS256"}},"tokens":{"warm":{"kid":"k1","claims":{"sub":"test-only"},"issuedOffsetSec":-10,"expOffsetSec":600,"signAtStep":0}}})).await.unwrap();
        assert!(t.values.is_empty());
        // Native RunnerTokens records the signing algorithm separately and does
        // not add an algorithm restriction to the published public JWK.
        assert!(t.keys["k1"].public.get("alg").is_none());
        t.sign(Some(0)).unwrap();
        let original = t.values["warm"].clone();
        t.sign(Some(1)).unwrap();
        assert_eq!(t.values["warm"], original);
        let mut v = json!(format!("Bearer {original}"));
        t.redact(&mut v);
        assert_eq!(v, "Bearer ${token:warm}");
        t.resolve(&mut v);
        assert_eq!(v, json!(format!("Bearer {original}")));
    }
}
