//! Second build stage (017 research R4, V173): reads the `CodeGeneratorRequest`
//! `capnp compile` produced for `schema/pharos.capnp` and emits, for every
//! interface, a `Handler` trait and the `Instrumented<S>` wrapper that is the
//! only implementation of the capnpc-generated `Server` trait. A handler can
//! therefore only become a capability through the wrapper, so no method can
//! exist unobserved.
//!
//! capnpc 0.27's `Server` methods have the shape
//! `fn m(self: Rc<Self>, MParams, MResults) -> impl Future<Output = Result<(), capnp::Error>> + 'static`
//! and take `Results` by value, which leaves no way to read the outcome after
//! the handler returns. `Handler` methods take `&mut MResults` instead, so the
//! wrapper keeps ownership and reads the `result` union back once the handler
//! is done.

use std::collections::HashMap;
use std::fmt::Write as _;

use std::collections::HashSet;

use capnp::schema_capnp::{code_generator_request, field, node, type_};

type GenResult<T> = Result<T, Box<dyn std::error::Error>>;

struct Method {
    /// Schema name, e.g. `nextUp`.
    name: String,
    /// Label and Rust fn name, e.g. `next_up`.
    label: String,
    /// `NextUp`, the prefix of capnpc's `NextUpParams` / `NextUpResults`.
    type_prefix: String,
    /// Whether a `$secret` field is reachable from its params or results.
    carries_secret: bool,
    /// Result fields holding a capability: `(field, interface label)`, e.g.
    /// `("library", "library")`. `Revoked` fills them with revoked capabilities.
    cap_results: Vec<(String, String)>,
}

struct Interface {
    /// Schema name, e.g. `UserData`.
    name: String,
    /// Label and capnpc module name, e.g. `user_data`.
    label: String,
    methods: Vec<Method>,
}

/// Same conversion capnpc uses for module and method names.
fn snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i != 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Rust keywords capnpc escapes with a trailing `_`; none occur in the schema
/// today, but a method named e.g. `type` must not silently mis-generate.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];

pub fn generate(request_bytes: &[u8]) -> GenResult<String> {
    let message = capnp::serialize::read_message(
        &mut &request_bytes[..],
        capnp::message::ReaderOptions::new(),
    )?;
    let request = message.get_root::<code_generator_request::Reader<'_>>()?;

    let mut nodes = HashMap::new();
    for n in request.get_nodes()? {
        nodes.insert(n.get_id(), n);
    }

    let mut interfaces = Vec::new();
    let mut method_structs = Vec::new();
    let mut secret_id = None;
    let mut interface_ids = HashMap::new();
    for file in request.get_requested_files()? {
        let Some(file_node) = nodes.get(&file.get_id()) else {
            return Err(format!("requested file {:#x} has no node", file.get_id()).into());
        };
        for nested in file_node.get_nested_nodes()? {
            let name = nested.get_name()?.to_str()?.to_string();
            let Some(n) = nodes.get(&nested.get_id()) else {
                continue;
            };
            match n.which()? {
                node::Interface(iface) => {
                    if !iface.get_superclasses()?.is_empty() {
                        return Err(format!("interface {name}: superclasses are not supported by the wrapper generator").into());
                    }
                    let mut methods = Vec::new();
                    for m in iface.get_methods()? {
                        let mname = m.get_name()?.to_str()?.to_string();
                        let label = snake(&mname);
                        if RUST_KEYWORDS.contains(&label.as_str()) {
                            return Err(format!(
                                "{name}.{mname}: keyword method names are not supported"
                            )
                            .into());
                        }
                        methods.push(Method {
                            type_prefix: capitalize(&mname),
                            label,
                            name: mname,
                            // Filled in once all nodes are known.
                            carries_secret: false,
                            cap_results: Vec::new(),
                        });
                        method_structs
                            .push((m.get_param_struct_type(), m.get_result_struct_type()));
                    }
                    interface_ids.insert(n.get_id(), snake(&name));
                    interfaces.push(Interface {
                        label: snake(&name),
                        name,
                        methods,
                    });
                }
                node::Annotation(_) if name == "secret" => secret_id = Some(n.get_id()),
                _ => {}
            }
        }
    }
    let Some(secret_id) = secret_id else {
        return Err("schema declares no `secret` annotation; redaction (V8) needs it".into());
    };
    let mut structs = method_structs.into_iter();
    for m in interfaces.iter_mut().flat_map(|i| i.methods.iter_mut()) {
        if let Some((params, results)) = structs.next() {
            m.carries_secret = reaches_secret(&nodes, params, secret_id, &mut HashSet::new())?
                || reaches_secret(&nodes, results, secret_id, &mut HashSet::new())?;
            m.cap_results = capability_fields(&nodes, &interface_ids, results)?;
        }
    }
    Ok(emit(&interfaces, secret_id))
}

/// The fields of struct `id` whose type is an interface, as
/// `(rust field name, interface label)`.
fn capability_fields(
    nodes: &HashMap<u64, node::Reader<'_>>,
    interface_ids: &HashMap<u64, String>,
    id: u64,
) -> GenResult<Vec<(String, String)>> {
    let Some(n) = nodes.get(&id) else {
        return Ok(Vec::new());
    };
    let node::Struct(st) = n.which()? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for f in st.get_fields()? {
        let field::Slot(slot) = f.which()? else {
            continue;
        };
        if let type_::Interface(t) = slot.get_type()?.which()? {
            let name = f.get_name()?.to_str()?;
            let Some(label) = interface_ids.get(&t.get_type_id()) else {
                return Err(format!(
                    "result field `{name}` holds a non-top-level interface; `Revoked` cannot fill it"
                )
                .into());
            };
            out.push((snake(name), label.clone()));
        }
    }
    Ok(out)
}

/// Whether struct `id`, or any struct, group or list of structs reachable
/// through its fields, has a field annotated `$secret`.
fn reaches_secret(
    nodes: &HashMap<u64, node::Reader<'_>>,
    id: u64,
    secret_id: u64,
    seen: &mut HashSet<u64>,
) -> GenResult<bool> {
    if !seen.insert(id) {
        return Ok(false);
    }
    let Some(n) = nodes.get(&id) else {
        return Ok(false);
    };
    let node::Struct(st) = n.which()? else {
        return Ok(false);
    };
    for f in st.get_fields()? {
        if f.get_annotations()?.iter().any(|a| a.get_id() == secret_id) {
            return Ok(true);
        }
        let nested = match f.which()? {
            field::Group(g) => Some(g.get_type_id()),
            field::Slot(slot) => {
                let mut ty = slot.get_type()?;
                while let type_::List(l) = ty.which()? {
                    ty = l.get_element_type()?;
                }
                match ty.which()? {
                    type_::Struct(s) => Some(s.get_type_id()),
                    _ => None,
                }
            }
        };
        if let Some(nested) = nested {
            if reaches_secret(nodes, nested, secret_id, seen)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn emit(interfaces: &[Interface], secret_id: u64) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "// Generated by crates/pharos-api/build/gen.rs from schema/pharos.capnp. Do not edit.\n"
    );
    let _ = writeln!(
        o,
        "/// Id of the schema's `$secret` annotation (V8, 017 FR-011)."
    );
    let _ = writeln!(
        o,
        "pub(crate) const SECRET_ANNOTATION_ID: u64 = {secret_id:#x};\n"
    );

    let _ = writeln!(
        o,
        "/// Every interface and method in the schema, in schema order."
    );
    let _ = writeln!(
        o,
        "pub static INTERFACES: &[crate::labels::InterfaceMeta] = &["
    );
    for i in interfaces {
        let _ = writeln!(
            o,
            "    crate::labels::InterfaceMeta {{ label: \"{}\", methods: &{}::METHODS }},",
            i.label, i.label
        );
    }
    let _ = writeln!(o, "];\n");

    let _ = writeln!(
        o,
        "/// `(interface, method)` of every method whose params or results reach a"
    );
    let _ = writeln!(
        o,
        "/// `$secret` field. The redaction test covers each one (V8)."
    );
    let _ = writeln!(o, "pub static SECRET_METHODS: &[(&str, &str)] = &[");
    for i in interfaces {
        for m in i.methods.iter().filter(|m| m.carries_secret) {
            let _ = writeln!(o, "    (\"{}\", \"{}\"),", i.label, m.label);
        }
    }
    let _ = writeln!(o, "];\n");

    for i in interfaces {
        emit_interface(&mut o, i);
    }
    o
}

fn emit_interface(o: &mut String, i: &Interface) {
    let target = format!("pharos_api::rpc::{}", i.label);
    let l = &i.label;
    let _ = writeln!(
        o,
        "/// `{}`: handler trait and instrumented wrapper.",
        i.name
    );
    let _ = writeln!(o, "pub mod {l} {{");
    let _ = writeln!(o, "    use crate::pharos_capnp::{l} as wire;");
    if i.methods.is_empty() {
        // No methods (e.g. `Subscription`, whose only behaviour is being
        // dropped): nothing to observe, but the capability still has to come
        // from `client`, so the same shape is kept.
        let _ = writeln!(o, "    use crate::instrument::Connection;");
        let _ = writeln!(o, "    use ::std::rc::Rc;\n");
        let _ = writeln!(
            o,
            "    pub(crate) static METHODS: [crate::labels::MethodMeta; 0] = [];\n"
        );
        let _ = writeln!(o, "    /// Implement this to serve `{}`.", i.name);
        let _ = writeln!(o, "    pub trait Handler: 'static {{}}\n");
        let _ = writeln!(o, "    pub struct Instrumented<S: Handler> {{");
        let _ = writeln!(o, "        _inner: Rc<S>,");
        let _ = writeln!(o, "    }}\n");
        let _ = writeln!(
            o,
            "    /// Wraps `handler` in [`Instrumented`] and returns the capability."
        );
        let _ = writeln!(
            o,
            "    pub fn client<S: Handler>(handler: S, _conn: &Rc<Connection>) -> wire::Client {{"
        );
        let _ = writeln!(
            o,
            "        ::capnp_rpc::new_client(Instrumented {{ _inner: Rc::new(handler) }})"
        );
        let _ = writeln!(o, "    }}\n");
        let _ = writeln!(
            o,
            "    impl<S: Handler> wire::Server for Instrumented<S> {{}}\n"
        );
        let _ = writeln!(
            o,
            "    /// The handler behind a revoked session's `{}`.",
            i.name
        );
        let _ = writeln!(o, "    pub struct Revoked(pub Rc<Connection>);");
        let _ = writeln!(o, "    impl Handler for Revoked {{}}");
        let _ = writeln!(o, "}}\n");
        return;
    }
    let _ = writeln!(
        o,
        "    use crate::instrument::{{CallRecord, CallScope, Connection, InterfaceLog}};"
    );
    let _ = writeln!(o, "    use ::std::rc::Rc;\n");

    let _ = writeln!(
        o,
        "    pub(crate) static METHODS: [crate::labels::MethodMeta; {}] = [",
        i.methods.len()
    );
    for m in &i.methods {
        let _ = writeln!(
            o,
            "        crate::labels::MethodMeta {{ interface: \"{l}\", method: \"{}\", span: \"rpc.{l}.{}\" }},",
            m.label, m.label
        );
    }
    let _ = writeln!(o, "    ];\n");

    // Log callsites need a literal target so `PHAROS_LOG_LEVEL` can toggle
    // one interface (`pharos_api::rpc::library=debug`, 017 §Observability).
    let fields = "interface = r.interface, method = r.method, call_id = %r.call_id, trace_id = %r.trace_id, device = %r.device, client_version = %r.client_version, user = %r.user, outcome = r.outcome, duration_ms = r.duration_ms, request_bytes = r.request_bytes, response_bytes = r.response_bytes";
    let _ = writeln!(
        o,
        "    fn log_failure(r: &CallRecord, reason: &str, detail: &str) {{"
    );
    let _ = writeln!(o, "        ::tracing::warn!(target: \"{target}\", {fields}, reason = %reason, detail = %detail, \"rpc call failed\");");
    let _ = writeln!(o, "    }}");
    let _ = writeln!(o, "    fn log_success(r: &CallRecord) {{");
    let _ = writeln!(
        o,
        "        ::tracing::debug!(target: \"{target}\", {fields}, \"rpc call ok\");"
    );
    let _ = writeln!(o, "    }}");
    let _ = writeln!(o, "    fn log_cancelled(r: &CallRecord) {{");
    let _ = writeln!(
        o,
        "        ::tracing::info!(target: \"{target}\", {fields}, \"rpc call cancelled\");"
    );
    let _ = writeln!(o, "    }}");
    let _ = writeln!(o, "    fn debug_enabled() -> bool {{");
    let _ = writeln!(
        o,
        "        ::tracing::enabled!(target: \"{target}\", ::tracing::Level::DEBUG)"
    );
    let _ = writeln!(o, "    }}");
    let _ = writeln!(
        o,
        "    fn log_payload(r: &CallRecord, direction: &'static str, payload: &str) {{"
    );
    let _ = writeln!(o, "        ::tracing::debug!(target: \"{target}\", interface = r.interface, method = r.method, call_id = %r.call_id, trace_id = %r.trace_id, direction, payload = %payload, \"rpc payload\");");
    let _ = writeln!(o, "    }}");
    let _ = writeln!(o, "    pub(crate) static LOG: InterfaceLog = InterfaceLog {{ failure: log_failure, success: log_success, cancelled: log_cancelled, debug_enabled, payload: log_payload }};\n");

    let _ = writeln!(
        o,
        "    /// Implement this to serve `{}`. Only [`client`] turns a handler into a",
        i.name
    );
    let _ = writeln!(
        o,
        "    /// capability, always through [`Instrumented`] (V173). Unimplemented"
    );
    let _ = writeln!(
        o,
        "    /// methods fail with `capnp::Error::unimplemented` and are counted."
    );
    let _ = writeln!(o, "    pub trait Handler: 'static {{");
    for m in &i.methods {
        let _ = writeln!(o, "        fn {}(", m.label);
        let _ = writeln!(o, "            self: Rc<Self>,");
        let _ = writeln!(o, "            params: wire::{}Params,", m.type_prefix);
        let _ = writeln!(
            o,
            "            results: &mut wire::{}Results,",
            m.type_prefix
        );
        let _ = writeln!(
            o,
            "        ) -> impl ::core::future::Future<Output = Result<(), ::capnp::Error>> {{"
        );
        let _ = writeln!(o, "            let _ = (params, results);");
        let _ = writeln!(o, "            ::core::future::ready(Err(::capnp::Error::unimplemented(\"{}.{} not implemented\".to_string())))", i.name, m.name);
        let _ = writeln!(o, "        }}");
    }
    let _ = writeln!(o, "    }}\n");

    let _ = writeln!(
        o,
        "    /// The only `wire::Server` implementation: every call opens"
    );
    let _ = writeln!(
        o,
        "    /// `rpc.{l}.<method>`, records RED metrics and logs its outcome."
    );
    let _ = writeln!(o, "    pub struct Instrumented<S: Handler> {{");
    let _ = writeln!(o, "        inner: Rc<S>,");
    let _ = writeln!(o, "        conn: Rc<Connection>,");
    let _ = writeln!(o, "    }}\n");
    let _ = writeln!(
        o,
        "    /// Wraps `handler` in [`Instrumented`] and returns the capability."
    );
    let _ = writeln!(
        o,
        "    pub fn client<S: Handler>(handler: S, conn: &Rc<Connection>) -> wire::Client {{"
    );
    let _ = writeln!(o, "        ::capnp_rpc::new_client(Instrumented {{ inner: Rc::new(handler), conn: Rc::clone(conn) }})");
    let _ = writeln!(o, "    }}\n");

    let _ = writeln!(
        o,
        "    impl<S: Handler> wire::Server for Instrumented<S> {{"
    );
    for (idx, m) in i.methods.iter().enumerate() {
        let _ = writeln!(o, "        fn {}(", m.label);
        let _ = writeln!(o, "            self: Rc<Self>,");
        let _ = writeln!(o, "            params: wire::{}Params,", m.type_prefix);
        let _ = writeln!(
            o,
            "            mut results: wire::{}Results,",
            m.type_prefix
        );
        let _ = writeln!(o, "        ) -> impl ::core::future::Future<Output = Result<(), ::capnp::Error>> + 'static {{");
        let _ = writeln!(o, "            let span = ::tracing::info_span!(target: \"{target}\", \"rpc.{l}.{}\", interface = \"{l}\", method = \"{}\", call_id = ::tracing::field::Empty, trace_id = ::tracing::field::Empty, device = ::tracing::field::Empty, client_version = ::tracing::field::Empty, ui_route = ::tracing::field::Empty, ui_action = ::tracing::field::Empty, user = ::tracing::field::Empty, outcome = ::tracing::field::Empty, request_bytes = ::tracing::field::Empty, response_bytes = ::tracing::field::Empty);", m.label, m.label);
        let _ = writeln!(o, "            let scope = CallScope::begin(&self.conn, &METHODS[{idx}], &LOG, span, params.get().map(::capnp::dynamic_value::Reader::from));");
        let _ = writeln!(o, "            let inner = Rc::clone(&self.inner);");
        let _ = writeln!(o, "            async move {{");
        let _ = writeln!(
            o,
            "                let outcome = scope.run(S::{}(inner, params, &mut results)).await;",
            m.label
        );
        let _ = writeln!(o, "                let reply = ::capnp::dynamic_value::Reader::from(results.get().into_reader());");
        let _ = writeln!(o, "                scope.finish(outcome, reply)");
        let _ = writeln!(o, "            }}");
        let _ = writeln!(o, "        }}");
    }
    let _ = writeln!(o, "    }}\n");
    emit_revoked(o, i);
    let _ = writeln!(o, "}}\n");
}

/// `Revoked`: what a refused or revoked session serves. Every call answers
/// `Error.kind.revoked` (counted `revoked`, not `unimplemented`); a capability
/// getter answers with a `Revoked` capability, because getters never fail and
/// `resume → library → home` must still pipeline (research R1).
fn emit_revoked(o: &mut String, i: &Interface) {
    let _ = writeln!(
        o,
        "    /// The handler behind a revoked session's `{}`: every call fails",
        i.name
    );
    let _ = writeln!(
        o,
        "    /// `Error.kind.revoked`; capability getters return `Revoked` capabilities."
    );
    let _ = writeln!(o, "    pub struct Revoked(pub Rc<Connection>);\n");
    let _ = writeln!(o, "    impl Handler for Revoked {{");
    for m in &i.methods {
        let _ = writeln!(o, "        fn {}(", m.label);
        let _ = writeln!(o, "            self: Rc<Self>,");
        let _ = writeln!(o, "            _params: wire::{}Params,", m.type_prefix);
        let _ = writeln!(
            o,
            "            results: &mut wire::{}Results,",
            m.type_prefix
        );
        let _ = writeln!(
            o,
            "        ) -> impl ::core::future::Future<Output = Result<(), ::capnp::Error>> {{"
        );
        if m.cap_results.is_empty() {
            let _ = writeln!(o, "            ::core::future::ready(crate::instrument::fill_revoked(::capnp::dynamic_value::Builder::from(results.get())))");
        } else {
            for (field, target) in &m.cap_results {
                let _ = writeln!(o, "            results.get().set_{field}(super::{target}::client(super::{target}::Revoked(Rc::clone(&self.0)), &self.0));");
            }
            let _ = writeln!(o, "            ::core::future::ready(Ok(()))");
        }
        let _ = writeln!(o, "        }}");
    }
    let _ = writeln!(o, "    }}");
}
