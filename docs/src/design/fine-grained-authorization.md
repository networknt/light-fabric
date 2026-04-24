# Fine-Grained Authorization

The following is the raw material collected when I chat with AI about migrating the light-4j fine-grained authorization to light-rs to secure light-agent and light-gateway. 

The information needs to be organized and documented into a design document for light-rs fine-grained authorization.

**Question:** We have a Java implementation of fine-grained authorization that we want to migrate to light-rs. In our java implementation, we have a middleware handler to verify the custom claims  like role, group, attribute etc in the jwt token against endpoint specific rules defined in database and cached at the service. How should we handle the same requirement in rust?

**Response:**

In Rust, bridging the gap between dynamic, database-driven rules and high-performance web requests is a well-solved problem. If you are coming from Java, the architecture translates very cleanly, but takes advantage of Rust's async runtime and thread-safe concurrency primitives.

To handle fine-grained authorization (Attribute-Based Access Control / ABAC or Role-Based Access Control / RBAC) where rules are cached locally but backed by a database, you have two primary paths in Rust:

1. **The Custom Middleware Approach** (Closest to your current Java implementation)
2. **The Policy Engine Approach** (Using an industry-standard Rust authorization crate)

Here is how you handle this requirement idiomatically in Rust, assuming you are using **Axum** (the current standard web framework in Rust).

---

### Approach 1: Custom Middleware with Async Caching

In Java, you likely used something like Guava or Caffeine for your local cache. In Rust, the exact equivalent for high-performance, concurrent, async-aware caching is the **`moka`** crate.

Here is the architectural blueprint:
1. **Define your custom claims** using `serde`.
2. **Setup a shared Application State** holding your Database connection pool (e.g., `sqlx`) and your local Cache (`moka`).
3. **Create an Axum Middleware** to intercept the request, parse the JWT, consult the cache based on the route, and authorize the request.

#### 1. Dependencies (`Cargo.toml`)
```toml
axum = "0.7"
jsonwebtoken = "9.0"
moka = { version = "0.12", features = ["future"] } # Async cache (Rust's Caffeine)
sqlx = { version = "0.7", features = ["postgres", "runtime-tokio"] }
serde = { version = "1.0", features = ["derive"] }
```

#### 2. Defining Claims and State
```rust
use axum::{extract::State, http::Request, middleware::Next, response::Response};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;

// 1. Define your custom JWT claims
#[derive(Debug, Serialize, Deserialize, Clone)]
struct CustomClaims {
    sub: String,
    roles: Vec<String>,
    groups: Vec<String>,
    attributes: std::collections::HashMap<String, String>,
    exp: usize,
}

// 2. Define the Endpoint Rule (what you fetch from DB)
#[derive(Clone)]
struct EndpointRule {
    required_roles: Vec<String>,
    // add required_groups, attributes, etc.
}

// 3. Define the Application State to share across your app
struct AppState {
    db: PgPool,
    // Cache keyed by String (e.g., "GET:/api/data") returning an EndpointRule
    rule_cache: Cache<String, EndpointRule>,
}
```

#### 3. The Authorization Middleware
In Axum, middleware is written as an asynchronous function that takes the request, does some work, and either rejects it or passes it to the `Next` handler.

```rust
use axum::http::StatusCode;
use axum::response::IntoResponse;

async fn fine_grained_authz_middleware(
    State(state): State<Arc<AppState>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    
    // 1. Extract and decode the JWT from Authorization header
    let auth_header = req.headers().get("Authorization").and_then(|h| h.to_str().ok());
    let claims = decode_jwt(auth_header).ok_or(StatusCode::UNAUTHORIZED)?;

    // 2. Identify the endpoint being accessed (e.g., "GET:/api/users/123")
    // Note: In practice, use req.extensions() to get the matched generic route template
    // rather than the raw URI so "/api/users/123" matches "/api/users/{id}"
    let endpoint_key = format!("{}:{}", req.method(), req.uri().path());

    // 3. Check Cache or Fallback to DB
    let rule = match state.rule_cache.get(&endpoint_key).await {
        Some(cached_rule) => cached_rule,
        None => {
            // Cache miss: Fetch from database
            let db_rule = fetch_rule_from_db(&state.db, &endpoint_key)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            
            // Populate cache
            state.rule_cache.insert(endpoint_key, db_rule.clone()).await;
            db_rule
        }
    };

    // 4. Execute Fine-Grained Authorization Logic
    let is_authorized = rule.required_roles.iter().any(|role| claims.roles.contains(role));
    // ... check groups, evaluate custom attribute expressions, etc.

    if !is_authorized {
        return Err(StatusCode::FORBIDDEN);
    }

    // 5. Pass to the actual route handler!
    Ok(next.run(req).await)
}

// Dummy helper functions to make the example compile mentally
fn decode_jwt(_header: Option<&str>) -> Option<CustomClaims> { /* ... */ unimplemented!() }
async fn fetch_rule_from_db(_db: &PgPool, _key: &str) -> Result<EndpointRule, ()> { /* ... */ unimplemented!() }
```

#### 4. Wiring it up
```rust
use axum::{routing::get, Router};

#[tokio::main]
async fn main() {
    let state = Arc::new(AppState {
        db: PgPool::connect("postgres://...").await.unwrap(),
        rule_cache: Cache::builder()
            .max_capacity(10_000)
            .time_to_live(std::time::Duration::from_secs(300)) // 5 minute TTL
            .build(),
    });

    let app = Router::new()
        .route("/api/secure-data", get(|| async { "Secret Data" }))
        // Apply the middleware to all routes above this line
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), fine_grained_authz_middleware))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

---

### Approach 2: Using a Dedicated Policy Engine (Recommended for complex rules)

If your endpoint-specific rules are highly complex (e.g., *"User must be in Group X AND have Attribute Y = 'finance', UNLESS Role is 'Admin'"*), writing that evaluation logic manually in Rust gets messy.

Instead of writing custom evaluation logic, the Rust ecosystem heavily relies on external Policy Engines for ABAC/RBAC. The two leaders are:

#### 1. AWS Cedar (`cedar-policy`) - *Highly Recommended*
Developed by Amazon Web Services in Rust, **Cedar** is a blazing-fast language for defining permissions.
* You store your Cedar policies in your database as text.
* Your Rust middleware fetches the Cedar policy string from the DB, compiles it (and caches the compiled policy using `moka`).
* You pass the JWT `CustomClaims` into the Cedar engine as a context entity.
* Cedar evaluates the claims against the policy in microseconds.

#### 2. Casbin (`casbin`)
Casbin is a very popular authorization library that exists in Java, Go, and Rust.
* It supports ACL, RBAC, and ABAC.
* It has native adapters for `sqlx` (Postgres/MySQL). You don't even need to write the caching logic yourself; `casbin-rs` handles reading the rules from the database and keeping them cached in memory automatically.

### Summary
* **For simple Role/Group matching:** Stick to the **Custom Middleware** approach using `moka` for async caching and `axum::middleware` to intercept requests. It is functionally identical to your Java approach but extremely fast.
* **For complex Attribute evaluation:** Use your middleware to parse the JWT, but delegate the actual rule evaluation to **`cedar-policy`** or **`casbin`**.


**Question:** Could you please provide some examples for cedar-policy and casbin? For example, what is the format in database? In our system, we are using a yaml rule engine to evaluate the fine-grained authorization. We create some rules users can choose and they can also create their own rules if necessary.


**Response:**




Transitioning from a custom YAML rule engine to an industry-standard policy engine is a fantastic move. Both **AWS Cedar** and **Casbin** are designed precisely for this kind of "user-defined fine-grained rules" requirement, but they take very different approaches to how rules are written and stored.

Here is a breakdown of how both engines work, what their database format looks like, and how you map your JWT claims to them in Rust.

---

### 1. AWS Cedar (`cedar-policy`) - Best for Complex/Custom Rules
Cedar uses a custom, highly readable Domain Specific Language (DSL). It is specifically built to evaluate ABAC (Attribute-Based Access Control) using contextual data like JWT claims.

#### The Format (In the Database)
In your database, you do not need complex tables. You simply store the Cedar policies as **plain text strings** in a standard `TEXT` or `VARCHAR` column.

If a user wants to create a custom rule saying, *"Allow access if the user's JWT has the 'finance' group and a clearance level greater than 3,"* the Cedar policy stored in the DB looks like this:

```cedar
// Database column: policy_text
permit (
    principal,
    action == Action::"read_data",
    resource == Resource::"secure_endpoint"
)
when {
    context.jwt.groups contains "finance" &&
    context.jwt.attributes.clearance_level > 3
};
```

For "predefined rules that users can choose," Cedar has **Policy Templates**. You store the template once, and users just link their parameters to it.
```cedar
// Template: "Require specific group"
permit(principal, action, resource)
when { context.jwt.groups contains ?required_group };
```

#### How you execute it in Rust
When a request comes in, you fetch the relevant Cedar text strings from your database, pass in the JWT claims as the `Context`, and let Cedar evaluate it.

```rust
use cedar_policy::{Authorizer, Context, Decision, Entities, PolicySet, Request};
use serde_json::json;
use std::str::FromStr;

fn evaluate_cedar_rule(db_policy_text: &str, jwt_claims: &serde_json::Value) {
    // 1. Parse the text policy from the database
    let policies = PolicySet::from_str(db_policy_text).expect("Invalid policy format");

    // 2. Map your JWT claims directly into the Cedar "Context" using JSON
    let context_json = json!({
        "jwt": jwt_claims
    });
    let context = Context::from_json_value(context_json, None).unwrap();

    // 3. Create the evaluation request
    let request = Request::new(
        Some("User::\"Alice\"".parse().unwrap()), 
        Some("Action::\"read_data\"".parse().unwrap()), 
        Some("Resource::\"secure_endpoint\"".parse().unwrap()), 
        context,
        None,
    ).unwrap();

    // 4. Evaluate
    let authorizer = Authorizer::new();
    let entities = Entities::empty(); // Used if fetching hierarchical data
    let answer = authorizer.is_authorized(&request, &policies, &entities);

    if answer.decision() == Decision::Allow {
        println!("Access Granted!");
    } else {
        println!("Access Denied. Failed policies: {:?}", answer.diagnostics().errors());
    }
}
```

---

### 2. Casbin (`casbin-rs`) - Best for Tabular/Database-Native Rules
Casbin works fundamentally differently. It splits authorization into two parts: the **Model** (a config file defining the structure of your rules) and the **Policies/Rules** (the actual data stored in the database).

#### The Format (In the Database)
Casbin doesn't store a DSL string. Instead, it natively connects to your database using an adapter (like `sqlx-adapter`) and creates a table usually called `casbin_rule`. 

The table structure looks exactly like a CSV file, with columns: `ptype`, `v0`, `v1`, `v2`, `v3`, `v4`, `v5`. 

If a user selects a predefined rule to allow the `finance_admin` role to read `/api/data`, Casbin inserts a row into your database like this:

| ptype | v0 (Subject)    | v1 (Object) | v2 (Action) | v3 (Condition - optional) |
|-------|-----------------|-------------|-------------|---------------------------|
| `p`   | `finance_admin` | `/api/data` | `read`      |                           |

If a user writes a **custom ABAC rule** using a UI (e.g., clearance > 3), Casbin can store the evaluation expression as a string in one of the columns (evaluated internally via the `rhai` scripting language):

| ptype | v0 (Subject/Condition)                                      | v1 (Object) | v2 (Action) |
|-------|-------------------------------------------------------------|-------------|-------------|
| `p`   | `r.sub.groups.contains("finance") && r.sub.clearance > 3`   | `/api/data` | `read`      |

#### How you execute it in Rust
To make Casbin evaluate JWT attributes dynamically, you define a `model.conf` file that tells Casbin to evaluate (`eval`) the string stored in the database.

**The `model.conf` File:**
```ini
[request_definition]
r = sub, obj, act

[policy_definition]
p = sub_rule, obj, act

[policy_effect]
e = some(where (p.eft == allow))

[matchers]
# This matcher says: check if the object and action match, 
# AND evaluate the ABAC rule stored in the database (p.sub_rule) against the request subject (r.sub)
m = r.obj == p.obj && r.act == p.act && eval(p.sub_rule) 
```

**The Rust Code:**
```rust
use casbin::{CoreApi, Enforcer, DefaultModel};
use sqlx_adapter::SqlxAdapter;
use std::collections::HashMap;

// A struct representing your JWT Claims
#[derive(Clone)]
struct JwtClaims {
    groups: Vec<String>,
    clearance: i32,
}

async fn evaluate_casbin() {
    // 1. Connect to DB. The adapter automatically fetches the rules from the `casbin_rule` table.
    let adapter = SqlxAdapter::new("postgres://user:pass@localhost/db", 8).await.unwrap();
    let mut enforcer = Enforcer::new("model.conf", adapter).await.unwrap();

    // 2. Parse the JWT for the incoming request
    let claims = JwtClaims {
        groups: vec!["finance".to_string()],
        clearance: 5,
    };

    // 3. Casbin in Rust uses `rhai` under the hood for dynamic object evaluation.
    // You map your JWT fields into a format the engine understands.
    let mut sub_map = HashMap::new();
    sub_map.insert("groups", claims.groups);
    sub_map.insert("clearance", claims.clearance);

    // 4. Enforce the rules against the DB
    // Request is: Subject (JWT Claims Map), Object (/api/data), Action (read)
    if enforcer.enforce((sub_map, "/api/data", "read")).unwrap() {
        println!("Access Granted!");
    } else {
        println!("Access Denied!");
    }
}
```

---

### Which one maps better to your YAML Rule Engine?

**AWS Cedar is highly recommended for your use case.**

Because you currently use a YAML rule engine where users define complex logical rules, your users are already accustomed to defining logical **expressions**.

1. **Translation:** It is very easy to write a script that takes your existing YAML definitions and converts them into Cedar text policies.
2. **Safety:** Cedar is statically typed and formally verified. If a user writes an invalid custom rule, Cedar will fail to parse it instantly and give you a helpful error message to show in your UI. Casbin's `eval()` logic can sometimes be brittle at runtime if the user types bad syntax.
3. **Database Schema:** With Cedar, you control your database schema. You just add a `policy_text` column to your existing "Rules" table. Casbin requires taking over a table (`casbin_rule`) to map to its strict CSV-like format.

**Question:** In our current implementation, we define all the persmissions and filters from UI and save the permissions and picked up rule in database. During server startup, it load everything locally in yaml format like the following. One of the pros is that our technical team write the rules, and the user or api owner just define the roles, groups, positions, attributes for the endpoint. The rule will be executed locally on the target server without going to the database so the performance is very good. If any permission changed for the target server, we can trigger a config reload from controller to ask the server to reload the config/permission. I am not sure if we should replace this implementation with Cedar. One of the feature we have is the column and row filter and I don't think Cedar support it. What do you think?

```
rule.endpointRules:
  echo@call:
    req-acc:
    - allow-role-based-access-control.lightapi.net
    permission:
      roles: mcp-reader
  /v1/pets@get:
    req-acc:
    - allow-role-based-access-control.lightapi.net
    permission:
      roles: host-admin
  /v1/accounts@get:
    req-acc:
    - allow-role-based-access-control.lightapi.net
    res-fil:
    - response-column-filter.lightapi.net
    - response-row-filter.lightapi.net
    permission:
      col:
        role:
          teller: '["accountNo","userId","accountType","firstName","lastName"]'
          account-manager: '["accountNo","userId","accountType","firstName","lastName","status"]'
      row:
        role:
          teller:
          - colName: accountType
            colValue: C
            operator: =
      roles: account-manager teller
```

**Response:**


You have excellent architectural instincts. **You are absolutely right to hesitate, and your concern about column and row filtering is the exact reason you probably should *not* replace your current system with Cedar.**

Your current design is actually highly sophisticated. Let’s break down why your current implementation is so strong, why Cedar would struggle with your specific requirements, and how you should idiomatically build your current design in Rust.

---

### Why Cedar is NOT a drop-in replacement for your system

AWS Cedar (and most standard policy engines like Casbin or OPA) are designed as **Binary Decision Engines**. You give them a Request, and they return an `Allow` or `Deny`. 

**The Missing Feature: Data Filtering (Masking)**
Your system handles two fundamentally different concerns in one elegant YAML file:
1.  **Access Control (`req-acc`):** *Can the user access the `/v1/accounts` endpoint?* (Cedar does this perfectly).
2.  **Data Filtering (`res-fil`):** *Which rows and columns can the user see in the response?* (Cedar **does not** do this natively).

If you forced Cedar to do column filtering, you would have to misuse its "Annotations" feature to attach custom metadata to policies, read the output, and then write a custom Rust engine to parse that metadata and filter the JSON anyway. You would essentially be rebuilding your existing system, just with Cedar awkwardly wedged in the middle.

### Strengths of your current architecture
What you have built is essentially a custom **API Gateway / Sidecar Data Masking Engine**. 
1.  **Performance:** Loading rules into memory at startup means zero network latency. (Matches Cedar).
2.  **Hot Reloading:** Fetching updates via a controller is standard cloud-native practice. (Matches Cedar).
3.  **Separation of Concerns:** Tech team writes the underlying engine logic (`allow-role-based-access-control`), while API owners just write YAML configs. (Matches Cedar’s Policy Templates).
4.  **Row/Column filtering:** Natively understood by your engine. (Far exceeds Cedar).

**Verdict:** Do not switch to Cedar. Your current YAML-based, locally cached evaluation engine is a perfect fit for your business requirements. 

---

### How to implement your architecture in Rust

Since you are migrating to or building in Rust, your current Java-based architecture translates beautifully. In fact, Rust will make this system significantly faster and more memory-efficient.

Here is how you replicate your exact Java architecture—including the hot-reloading and filtering—in Rust.

#### 1. Hot-Reloading using `arc-swap`
In Java, hot-reloading configuration safely across threads can be tricky. In Rust, the industry standard for "read-heavy, occasionally-updated" memory structures is the **`arc-swap`** crate. It allows your web server to read the YAML rules lock-free at millions of requests per second, while your controller can swap out the rules in the background instantly.

```rust
use arc_swap::ArcSwap;
use serde::Deserialize;
use std::sync::Arc;
use std::collections::HashMap;

// 1. Map your YAML exactly to Rust Structs
#[derive(Deserialize, Debug, Clone)]
struct EndpointRule {
    #[serde(rename = "req-acc")]
    req_acc: Option<Vec<String>>,
    #[serde(rename = "res-fil")]
    res_fil: Option<Vec<String>>,
    permission: PermissionConfig,
}

#[derive(Deserialize, Debug, Clone)]
struct PermissionConfig {
    roles: String,
    col: Option<HashMap<String, HashMap<String, String>>>, // Map for roles to column JSON strings
}

// 2. Wrap your entire config in an ArcSwap
lazy_static::lazy_static! {
    static ref CONFIG: ArcSwap<HashMap<String, EndpointRule>> = ArcSwap::from_pointee(HashMap::new());
}

// 3. Background task to listen to the Controller and Hot-Reload
async fn reload_config_from_controller(new_yaml_str: &str) {
    let new_config: HashMap<String, EndpointRule> = serde_yaml::from_str(new_yaml_str).unwrap();
    // Swaps the pointer atomically. 
    // All new requests see the new rules instantly. Old requests finish using the old rules.
    CONFIG.store(Arc::new(new_config));
}
```

#### 2. The Request Access (`req-acc`) Middleware
You would write an Axum middleware that intercepts the request, looks up the current endpoint in the `arc-swap` configuration, checks the JWT roles against `permission.roles`, and rejects the request if it fails.

```rust
// Inside your Axum middleware
let current_config = CONFIG.load();
if let Some(rule) = current_config.get("/v1/accounts@get") {
    let required_roles: Vec<&str> = rule.permission.roles.split_whitespace().collect();
    if !user_roles.iter().any(|r| required_roles.contains(&r.as_str())) {
        return Err(StatusCode::FORBIDDEN); // req-acc fails
    }
}
```

#### 3. The Response Filtering (`res-fil`) Middleware
This is where Rust shines. Because Axum and Actix allow you to intercept the *response* body after the handler completes but before it goes to the user, you can apply your column filtering dynamically.

Using `serde_json`, you can strip out columns that the user isn't allowed to see based on the `col` mapping in your YAML.

```rust
use serde_json::Value;

fn apply_column_filter(mut response_json: Value, allowed_columns_json: &str) -> Value {
    // 1. Parse the allowed columns from the YAML string (e.g., '["accountNo","firstName"]')
    let allowed_cols: Vec<String> = serde_json::from_str(allowed_columns_json).unwrap();

    // 2. Filter the JSON object
    if let Value::Object(ref mut map) = response_json {
        // Retain only the keys that are in the allowed_cols list
        map.retain(|key, _| allowed_cols.contains(key));
    }
    
    response_json
}
```

### Summary
Keep your current system! 
1. **The YAML approach is great.** It acts as a perfect declarative contract between your API owners and your technical rules engine.
2. **Cedar is the wrong tool for Data Masking.** Cedar is for strict Allow/Deny decisions, not mutating response bodies or filtering data.
3. **Rust will supercharge this.** By using `serde_yaml` to parse your config, `arc-swap` for lock-free hot-reloading, and `serde_json` for column masking inside an Axum middleware, you will get a system that does exactly what your Java system does, but with significantly less memory overhead and higher throughput.

