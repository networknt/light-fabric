//! Immutable operation descriptors compiled from a trusted local document.
//!
//! This is the metadata stage, not request validation or authentication. It
//! neither loads a document nor resolves remote references. There is no
//! Deserialize/header constructor for an operation descriptor: the configuration
//! loader owns provenance, as opposed to untrusted invocation fields.
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    OpenApi30,
    OpenApi31,
}
pub type SecurityRequirement = BTreeMap<String, Vec<String>>;

mod base_path;
mod ordered_source;
pub use base_path::{
    BasePathProfile, MAX_NORMALIZATION_PATH_BYTES, NormalizationInputTooLarge, PROFILE_ERROR,
    compile_router_capture_candidate,
};
pub use ordered_source::{MAX_SOURCE_BYTES, SourceFormat};

pub struct OperationDescriptor {
    dialect: Dialect,
    method: String,
    path_template: String,
    operation_id: Option<String>,
    security: Option<Arc<[SecurityRequirement]>>,
    oauth2_order: Option<Arc<[String]>>,
    document: Arc<Value>,
    path_item: Arc<Value>,
    operation: Arc<Value>,
}
impl OperationDescriptor {
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }
    pub fn method(&self) -> &str {
        &self.method
    }
    pub fn path_template(&self) -> &str {
        &self.path_template
    }
    pub fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_deref()
    }
    /// Preserve alternative requirement objects (OR), schemes within each
    /// object (AND), and explicit empty arrays, without activating any factor.
    pub fn security_requirements(&self) -> Option<&[SecurityRequirement]> {
        self.security.as_deref()
    }
    /// None denotes a metadata-only Value caller whose source order is unknown.
    /// An empty qualified slice means the source declared no OAuth2 schemes.
    pub fn ordered_oauth2_names(&self) -> Option<&[String]> {
        self.oauth2_order.as_deref()
    }
    pub fn document(&self) -> &Value {
        &self.document
    }
    pub fn path_item(&self) -> &Value {
        &self.path_item
    }
    pub fn operation(&self) -> &Value {
        &self.operation
    }
}

#[derive(Clone)]
pub struct OperationCatalogue {
    paths: BTreeMap<String, BTreeMap<String, Arc<OperationDescriptor>>>,
    source: Option<(SourceFormat, Arc<[u8]>)>,
}
impl OperationCatalogue {
    /// Takes an independent configuration snapshot. No source paths, credentials,
    /// clients, clocks, host discovery, or runtime/provider types are consulted.
    pub fn from_document(value: &Value) -> Result<Self, &'static str> {
        Self::construct(value, None)
    }
    /// Trusted local source only. Capture order before converting object maps.
    pub fn from_source(source: &[u8], format: SourceFormat) -> Result<Self, &'static str> {
        let (value, order) = ordered_source::parse(source, format)?;
        let mut catalogue = Self::construct(&value, Some(order.into()))?;
        // Retain original lexemes, independently of ordinary metadata numbers.
        // This copies the admitted capture; it never reopens a source.
        catalogue.source = Some((format, Arc::from(source)));
        Ok(catalogue)
    }
    /// Immutable original source for isolated consumers requiring exact lexemes.
    /// Metadata-only callers deliberately have no qualified source resource.
    pub fn source_resource(&self) -> Option<(SourceFormat, Arc<[u8]>)> {
        self.source.clone()
    }
    /// Opt-in bridge for a trusted isolated parser's numeric-neutral metadata.
    /// The projection is for operation/security metadata only. Exact validation
    /// must use the retained original capture, never this projected document.
    /// Neither input is a filename; this entry point performs no source I/O.
    pub fn from_captured_source_metadata(
        source: Arc<[u8]>,
        format: SourceFormat,
        metadata_json: &[u8],
    ) -> Result<Self, &'static str> {
        if source.len() > MAX_SOURCE_BYTES {
            return Err("specification: source byte limit exceeded");
        }
        let (value, order) = ordered_source::parse_projection(metadata_json)?;
        let mut catalogue = Self::construct(&value, Some(order.into()))?;
        catalogue.source = Some((format, source));
        Ok(catalogue)
    }
    fn construct(value: &Value, order: Option<Arc<[String]>>) -> Result<Self, &'static str> {
        let root = value
            .as_object()
            .ok_or("specification: document object required")?;
        let version = root
            .get("openapi")
            .and_then(Value::as_str)
            .ok_or("specification: version required")?;
        let parts: Vec<_> = version.split('.').collect();
        if parts.len() != 3 || parts[2].parse::<u32>().is_err() {
            return Err("specification: unsupported version");
        }
        let dialect = match (parts[0], parts[1]) {
            ("3", "0") => Dialect::OpenApi30,
            ("3", "1") => Dialect::OpenApi31,
            _ => return Err("specification: unsupported version"),
        };
        let document = Arc::new(value.clone());
        let inherited = security(root.get("security"))?.map(Arc::<[SecurityRequirement]>::from);
        let input = root
            .get("paths")
            .and_then(Value::as_object)
            .ok_or("specification: paths object required")?;
        let mut paths = BTreeMap::new();
        for (template, item) in input {
            if template.starts_with("x-") {
                continue;
            }
            let item = local_path_item(&document, item)?;
            let object = item
                .as_object()
                .ok_or("specification: path item object required")?;
            let path_item = Arc::new(item.clone());
            let mut methods = BTreeMap::new();
            for method in [
                "get", "put", "post", "delete", "options", "head", "patch", "trace",
            ] {
                let Some(operation) = object.get(method) else {
                    continue;
                };
                let operation_object = operation
                    .as_object()
                    .ok_or("specification: operation object required")?;
                let id = match operation_object.get("operationId") {
                    None => None,
                    Some(Value::String(id)) => Some(id.clone()),
                    _ => return Err("specification: invalid operation id"),
                };
                let requirements = if operation_object.contains_key("security") {
                    security(operation_object.get("security"))?
                        .map(Arc::<[SecurityRequirement]>::from)
                } else {
                    inherited.clone()
                };
                methods.insert(
                    method.to_ascii_uppercase(),
                    Arc::new(OperationDescriptor {
                        dialect,
                        method: method.to_ascii_uppercase(),
                        path_template: template.clone(),
                        operation_id: id,
                        security: requirements,
                        oauth2_order: order.clone(),
                        document: document.clone(),
                        path_item: path_item.clone(),
                        operation: Arc::new(operation.clone()),
                    }),
                );
            }
            paths.insert(template.clone(), methods);
        }
        Ok(Self {
            paths,
            source: None,
        })
    }
    pub fn templates(&self) -> impl Iterator<Item = &str> {
        self.paths.keys().map(String::as_str)
    }
    pub fn operation(&self, template: &str, method: &str) -> Option<Arc<OperationDescriptor>> {
        self.paths
            .get(template)?
            .get(&method.to_ascii_uppercase())
            .cloned()
    }
}

fn security(value: Option<&Value>) -> Result<Option<Vec<SecurityRequirement>>, &'static str> {
    let Some(value) = value else {
        return Ok(None);
    };
    let requirements = value
        .as_array()
        .ok_or("specification: security array required")?;
    let mut output = Vec::new();
    for requirement in requirements {
        let map = requirement
            .as_object()
            .ok_or("specification: security requirement object required")?;
        let mut schemes = BTreeMap::new();
        for (name, scopes) in map {
            if name.is_empty() {
                return Err("specification: invalid security scheme name");
            }
            let scopes = scopes
                .as_array()
                .ok_or("specification: scope array required")?;
            let scopes = scopes
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(String::from)
                        .ok_or("specification: scope string required")
                })
                .collect::<Result<Vec<_>, _>>()?;
            schemes.insert(name.clone(), scopes);
        }
        output.push(schemes);
    }
    Ok(Some(output))
}
fn local_path_item<'a>(
    document: &'a Value,
    mut item: &'a Value,
) -> Result<&'a Value, &'static str> {
    let mut seen = BTreeSet::new();
    while let Some(reference) = item.get("$ref") {
        let reference = reference
            .as_str()
            .ok_or("specification: invalid path reference")?;
        let pointer = reference
            .strip_prefix('#')
            .filter(|pointer| pointer.starts_with('/'))
            .ok_or("specification: only document-local path references are permitted")?;
        if !seen.insert(reference) {
            return Err("specification: cyclic path reference");
        }
        // Conflicting Path Item siblings have undefined 3.0/3.1 semantics;
        // metadata preparation does not merge them or fetch another source.
        if item.as_object().is_some_and(|map| {
            map.keys()
                .any(|key| !matches!(key.as_str(), "$ref" | "summary" | "description"))
        }) {
            return Err("specification: conflicting path reference siblings");
        }
        item = document
            .pointer(pointer)
            .ok_or("specification: missing local path reference")?;
    }
    Ok(item)
}

#[cfg(test)]
mod immutable_sharing_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inherited_requirements_and_path_items_share_only_within_init() {
        let mut source = json!({"openapi":"3.1.0", "security":[{"oauth":["read"],"other":[]},{}],
            "paths":{"/x":{"get":{},"post":{},"put":{"security":[]},
                "delete":{"security":[{"oauth":["write"]}]}}}});
        let first = OperationCatalogue::from_document(&source).unwrap();
        let second = OperationCatalogue::from_document(&source).unwrap();
        let get = first.operation("/x", "get").unwrap();
        let post = first.operation("/x", "post").unwrap();
        let put = first.operation("/x", "put").unwrap();
        let delete = first.operation("/x", "delete").unwrap();
        assert!(Arc::ptr_eq(
            get.security.as_ref().unwrap(),
            post.security.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(&get.path_item, &post.path_item));
        assert!(Arc::ptr_eq(&get.path_item, &put.path_item));
        assert!(!Arc::ptr_eq(
            get.security.as_ref().unwrap(),
            delete.security.as_ref().unwrap()
        ));
        assert!(!Arc::ptr_eq(
            &get.path_item,
            &second.operation("/x", "get").unwrap().path_item
        ));
        assert!(!Arc::ptr_eq(
            get.security.as_ref().unwrap(),
            second
                .operation("/x", "get")
                .unwrap()
                .security
                .as_ref()
                .unwrap()
        ));
        assert_eq!(get.security_requirements().unwrap().len(), 2);
        assert_eq!(get.security_requirements().unwrap()[0]["oauth"], ["read"]);
        assert!(get.security_requirements().unwrap()[1].is_empty());
        assert!(put.security_requirements().unwrap().is_empty());
        assert_eq!(
            delete.security_requirements().unwrap()[0]["oauth"],
            ["write"]
        );
        source["security"][0]["oauth"][0] = json!("mutated");
        assert_eq!(get.security_requirements().unwrap()[0]["oauth"], ["read"]);
        let absent = OperationCatalogue::from_document(
            &json!({"openapi":"3.0.3","paths":{"/x":{"get":{}}}}),
        )
        .unwrap();
        assert!(
            absent
                .operation("/x", "get")
                .unwrap()
                .security_requirements()
                .is_none()
        );
    }
}
