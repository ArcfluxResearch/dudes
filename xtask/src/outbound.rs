//! Static outbound-network regression controls for the shipped binaries.
//!
//! The source scan recognizes known paths through file-wide import and type-alias candidates and
//! local qualifiers. It does not resolve Rust namespaces or types. It reads every Rust file
//! under the reachable workspace members' `src` directories, including out-of-line test files.
//! Inline items and whole files whose `cfg` requires tests are skipped. Other configurations are
//! scanned together. Macro tokens expose concrete paths, but synthesized paths, reexports and
//! qualified members through user-defined modules, implicit hostname lookup through listener APIs
//! and arbitrary FFI remain outside this check. Dependency bans cover known clients, not every
//! possible client implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use proc_macro2::{Span, TokenStream, TokenTree};
use serde_json::Value;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Attribute, Item, Meta, Token, Type, UseTree};

use crate::metadata::{self, Graph};

pub(super) const BINARIES: &[&str] = &["duded", "dudes", "dudes-hook"];

pub(super) const FORBIDDEN_DEPS: &[&str] = &[
    "async-net",
    "awc",
    "curl",
    "curl-sys",
    "hickory-resolver",
    "hyper",
    "hyper-util",
    "isahc",
    "native-tls",
    "openssl",
    "openssl-sys",
    "reqwest",
    "rustls",
    "socket2",
    "surf",
    "trust-dns-resolver",
    "tungstenite",
    "ureq",
];

const CAPABILITIES: &[(&[&str], &[&str], &str)] = &[
    (
        &["std", "net", "TcpStream"],
        &["connect", "connect_timeout"],
        "outbound TCP",
    ),
    (&["std", "net", "UdpSocket"], &[], "UDP socket"),
    (&["std", "net", "ToSocketAddrs"], &[], "DNS lookup"),
    (&["libc"], &["connect"], "raw socket connect"),
    (&["libc"], &["sendto", "sendmsg"], "raw socket send"),
    (&["libc"], &["getaddrinfo", "gethostbyname"], "DNS lookup"),
    (
        &["nix", "sys", "socket"],
        &["connect"],
        "raw socket connect",
    ),
    (
        &["nix", "sys", "socket"],
        &["sendto", "sendmsg"],
        "raw socket send",
    ),
    (
        &["windows_sys", "Win32", "Networking", "WinSock"],
        &[
            "connect",
            "WSAConnect",
            "sendto",
            "WSASendTo",
            "getaddrinfo",
            "GetAddrInfoW",
            "gethostbyname",
        ],
        "WinSock outbound/DNS",
    ),
];

type Aliases = BTreeMap<String, BTreeSet<Vec<String>>>;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Violation {
    line: usize,
    capability: &'static str,
}

pub(super) fn check(metadata: &Value, graph: &Graph) -> Result<()> {
    let mut count = 0;
    for path in source_paths(metadata, graph)? {
        let source =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        for violation in
            violations(&source).with_context(|| format!("cannot inspect {}", path.display()))?
        {
            count += 1;
            println!(
                "layering: outbound VIOLATION {}:{} ({})",
                path.display(),
                violation.line,
                violation.capability
            );
        }
    }
    if count > 0 {
        bail!("{count} outbound-network source violation(s)");
    }
    println!("layering: outbound source ok");
    Ok(())
}

fn source_paths(metadata: &Value, graph: &Graph) -> Result<Vec<PathBuf>> {
    let root = Path::new(
        metadata["workspace_root"]
            .as_str()
            .context("no workspace_root")?,
    );
    let mut paths = BTreeSet::new();
    for package in metadata::members(metadata)? {
        let reachable = BINARIES.iter().try_fold(false, |found, binary| {
            let Some(start) = graph.member_id(binary) else {
                return Ok::<_, anyhow::Error>(found);
            };
            Ok(found || *binary == package.name || graph.path_to(start, &package.name)?.is_some())
        })?;
        if reachable {
            let manifest = root.join(package.manifest);
            let src = manifest
                .parent()
                .context("manifest has no parent")?
                .join("src");
            collect_sources(&src, &mut paths)?;
        }
    }
    Ok(paths.into_iter().collect())
}

fn collect_sources(dir: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_sources(&path, paths)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            paths.insert(path);
        }
    }
    Ok(())
}

fn violations(source: &str) -> Result<Vec<Violation>> {
    let file = syn::parse_file(source).context("invalid Rust source")?;
    if test_only(&file.attrs) {
        return Ok(Vec::new());
    }
    let mut collector = Collector {
        aliases: Aliases::new(),
        violations: BTreeSet::new(),
    };
    let mut imports = Imports(&mut collector.aliases);
    imports.visit_file(&file);
    collector.visit_file(&file);
    Ok(collector.violations.into_iter().collect())
}

struct Imports<'a>(&'a mut Aliases);

impl<'ast> Visit<'ast> for Imports<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if !test_only(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        import_paths(&item.tree, &[], self.0, true);
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        if !test_only(impl_attrs(item)) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        if !test_only(trait_attrs(item)) {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_expr_block(&mut self, block: &'ast syn::ExprBlock) {
        if !test_only(&block.attrs) {
            visit::visit_expr_block(self, block);
        }
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        if let Type::Path(path) = &*item.ty {
            self.0
                .entry(item.ident.to_string())
                .or_default()
                .insert(segments(&path.path));
        }
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        if let Some((_, rename)) = &item.rename {
            self.0
                .entry(rename.to_string())
                .or_default()
                .insert(vec![item.ident.to_string()]);
        }
    }
}

fn import_paths(tree: &UseTree, prefix: &[String], aliases: &mut Aliases, expand_globs: bool) {
    match tree {
        UseTree::Path(path) => {
            let mut prefix = prefix.to_vec();
            prefix.push(path.ident.to_string());
            import_paths(&path.tree, &prefix, aliases, expand_globs);
        }
        UseTree::Name(name) => {
            let mut path = prefix.to_vec();
            let ident = name.ident.to_string();
            if ident != "self" {
                path.push(ident.clone());
            }
            if let Some(name) = path.last() {
                aliases.entry(name.clone()).or_default().insert(path);
            }
        }
        UseTree::Rename(rename) => {
            let mut path = prefix.to_vec();
            if rename.ident != "self" {
                path.push(rename.ident.to_string());
            }
            aliases
                .entry(rename.rename.to_string())
                .or_default()
                .insert(path);
        }
        UseTree::Group(group) => {
            for tree in &group.items {
                import_paths(tree, prefix, aliases, expand_globs);
            }
        }
        UseTree::Glob(_) => {
            if expand_globs {
                let names: BTreeSet<_> = CAPABILITIES
                    .iter()
                    .flat_map(|(path, operations, _)| {
                        path.last()
                            .filter(|_| path.len() > 1)
                            .copied()
                            .into_iter()
                            .chain(operations.iter().copied())
                    })
                    .collect();
                for name in names {
                    let mut path = prefix.to_vec();
                    path.push(name.to_owned());
                    aliases.entry(name.to_owned()).or_default().insert(path);
                }
            }
        }
    }
}

struct Collector {
    aliases: Aliases,
    violations: BTreeSet<Violation>,
}

impl Collector {
    fn record(&mut self, span: Span, capability: &'static str) {
        self.violations.insert(Violation {
            line: span.start().line,
            capability,
        });
    }

    fn check_path(&mut self, path: Vec<String>, span: Span) {
        let mut pending = vec![(path, BTreeSet::new())];
        while let Some((path, mut seen)) = pending.pop() {
            let path = path
                .into_iter()
                .skip_while(|segment| matches!(segment.as_str(), "self" | "crate" | "super"))
                .collect::<Vec<_>>();
            let Some(first) = path.first() else { continue };
            for (prefix, operations, capability) in CAPABILITIES {
                if path
                    .iter()
                    .map(String::as_str)
                    .take(prefix.len())
                    .eq(prefix.iter().copied())
                    && (operations.is_empty()
                        || path
                            .get(prefix.len())
                            .is_some_and(|operation| operations.contains(&operation.as_str())))
                {
                    self.record(span, capability);
                }
            }
            let Some(aliases) = self.aliases.get(first) else {
                continue;
            };
            let aliases = aliases.clone();
            if !seen.insert(first.clone()) {
                continue;
            }
            for alias in &aliases {
                let expanded = alias.iter().chain(&path[1..]).cloned().collect();
                pending.push((expanded, seen.clone()));
            }
        }
    }

    fn tokens(&mut self, stream: TokenStream) {
        let tokens: Vec<_> = stream.into_iter().collect();
        for (at, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                self.tokens(group.stream());
            }
            let TokenTree::Ident(ident) = token else {
                continue;
            };
            let mut path = vec![ident.to_string()];
            let mut next = at + 1;
            loop {
                if matches!(tokens.get(next), Some(TokenTree::Punct(punct)) if punct.as_char() == '>')
                {
                    next += 1;
                }
                let Some(
                    [
                        TokenTree::Punct(a),
                        TokenTree::Punct(b),
                        TokenTree::Ident(ident),
                    ],
                ) = tokens.get(next..next + 3)
                else {
                    break;
                };
                if a.as_char() != ':' || b.as_char() != ':' {
                    break;
                }
                path.push(ident.to_string());
                next += 3;
            }
            self.check_path(path, ident.span());
            if ident == "to_socket_addrs" {
                self.record(ident.span(), "DNS lookup");
            }
        }
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item(&mut self, item: &'ast Item) {
        if !test_only(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.check_path(segments(path), path.span());
        visit::visit_path(self, path);
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        if !test_only(impl_attrs(item)) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        if !test_only(trait_attrs(item)) {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_expr_block(&mut self, block: &'ast syn::ExprBlock) {
        if !test_only(&block.attrs) {
            visit::visit_expr_block(self, block);
        }
    }

    fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
        if let Some(qself) = &expr.qself
            && let Type::Path(ty) = &*qself.ty
        {
            let path = segments(&ty.path)
                .into_iter()
                .chain(segments(&expr.path))
                .collect();
            self.check_path(path, expr.span());
        }
        visit::visit_expr_path(self, expr);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let mut aliases = Aliases::new();
        import_paths(&item.tree, &[], &mut aliases, false);
        for path in aliases.into_values().flatten() {
            self.check_path(path, item.span());
        }
    }

    fn visit_expr_method_call(&mut self, expr: &'ast syn::ExprMethodCall) {
        if expr.method == "to_socket_addrs" {
            self.record(expr.method.span(), "DNS lookup");
        }
        visit::visit_expr_method_call(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.tokens(mac.tokens.clone());
    }
}

fn segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

fn impl_attrs(item: &syn::ImplItem) -> &[Attribute] {
    match item {
        syn::ImplItem::Const(item) => &item.attrs,
        syn::ImplItem::Fn(item) => &item.attrs,
        syn::ImplItem::Type(item) => &item.attrs,
        syn::ImplItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

fn trait_attrs(item: &syn::TraitItem) -> &[Attribute] {
    match item {
        syn::TraitItem::Const(item) => &item.attrs,
        syn::TraitItem::Fn(item) => &item.attrs,
        syn::TraitItem::Type(item) => &item.attrs,
        syn::TraitItem::Macro(item) => &item.attrs,
        _ => &[],
    }
}

fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<Meta>()
                .is_ok_and(|meta| requires_test(&meta))
    })
}

fn requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) => {
            let Ok(children) =
                list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                return false;
            };
            if list.path.is_ident("all") {
                children.iter().any(requires_test)
            } else if list.path.is_ident("any") {
                !children.is_empty() && children.iter().all(requires_test)
            } else {
                false
            }
        }
        Meta::NameValue(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn outbound_tcp_reports_the_call_location() {
        assert_eq!(
            violations("fn run() {\n std::net::TcpStream::connect(\"127.0.0.1:9\");\n}").unwrap(),
            vec![Violation {
                line: 2,
                capability: "outbound TCP"
            }]
        );
    }

    #[test]
    fn tcp_import_spellings_aliases_and_function_values_are_rejected() {
        let sources = [
            "use std::net::TcpStream; fn run() { TcpStream::connect(\"127.0.0.1:9\"); }",
            "use std::net::{TcpStream as Stream, TcpListener}; fn run() { Stream::connect(\"127.0.0.1:9\"); }",
            "use std::net as sockets; fn run() { sockets::TcpStream::connect_timeout(addr, timeout); }",
            "use std::{net as sockets}; use sockets::TcpStream as Stream; type Conn = Stream; fn run() { Conn::connect(\"127.0.0.1:9\"); }",
            "use std::net::*; fn run() { TcpStream::connect(\"127.0.0.1:9\"); }",
            "use std::net::TcpStream; type Conn = TcpStream; fn run() { let dial = Conn::connect; }",
            "type Conn = std::net::TcpStream; fn run() { let dial = <Conn>::connect; }",
            "type Conn = std::net::TcpStream; fn run() { self::Conn::connect(addr); }",
            "extern crate std as standard; fn run() { standard::net::TcpStream::connect(\"127.0.0.1:9\"); }",
            "use std; fn run() { std::net::TcpStream::connect(addr); }",
            "use std::{self, net::TcpStream}; fn run() { std::net::TcpStream::connect(addr); }",
        ];
        for source in sources {
            assert_eq!(
                violations(source).unwrap(),
                vec![Violation {
                    line: 1,
                    capability: "outbound TCP"
                }],
                "{source}"
            );
        }
    }

    #[test]
    fn udp_and_dns_capabilities_are_rejected() {
        for (source, capability) in [
            (
                "use std::net::UdpSocket as Datagram; fn run() { Datagram::bind(addr); }",
                "UDP socket",
            ),
            ("use std::net::ToSocketAddrs as Resolve;", "DNS lookup"),
            (
                "fn run() { \"example.invalid:443\".to_socket_addrs(); }",
                "DNS lookup",
            ),
            (
                "fn run() { libc::getaddrinfo(host, service, hint, result); }",
                "DNS lookup",
            ),
            (
                "use nix::sys::socket::connect as dial; fn run() { dial(fd, addr); }",
                "raw socket connect",
            ),
            (
                "use nix::sys::socket::*; fn run() { connect(fd, addr); }",
                "raw socket connect",
            ),
            (
                "use libc::*; fn run() { getaddrinfo(host, service, hint, result); }",
                "DNS lookup",
            ),
            (
                "use windows_sys::Win32::Networking::WinSock::*; fn run() { WSAConnect(socket, addr, len, ptr, ptr, ptr, ptr); }",
                "WinSock outbound/DNS",
            ),
            (
                "use windows_sys::Win32::Networking::WinSock as sockets; fn run() { sockets::WSAConnect(socket, addr, len, ptr, ptr, ptr, ptr); }",
                "WinSock outbound/DNS",
            ),
        ] {
            assert_eq!(
                violations(source).unwrap(),
                vec![Violation {
                    line: 1,
                    capability
                }],
                "{source}"
            );
        }
    }

    #[test]
    fn local_listeners_accepted_streams_and_ipc_are_allowed() {
        let source = r#"
            use std::net::{TcpListener, TcpStream, SocketAddr};
            use std::os::unix::net::UnixStream;
            fn listen(addr: SocketAddr) { let listener = TcpListener::bind(addr); listener.accept(); }
            fn accepted(stream: TcpStream) { stream.write_all(b"ok"); }
            fn local() { UnixStream::connect("/tmp/control.sock"); }
            fn credentials() { nix::sys::socket::getsockopt(fd, option); }
            fn pipe() { std::fs::OpenOptions::new().open(r"\\.\pipe\control"); }
        "#;
        assert!(violations(source).unwrap().is_empty());
        assert!(
            violations(
                "use std::net::*; fn accepted(stream: TcpStream) { stream.write_all(b\"ok\"); }"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn concrete_macro_paths_are_scanned_but_literals_and_comments_are_ignored() {
        let bad =
            "use std::net::TcpStream as Stream; fn run() { wrap!({ Stream::connect(addr) }); }";
        assert_eq!(
            violations(bad).unwrap(),
            vec![Violation {
                line: 1,
                capability: "outbound TCP"
            }]
        );
        for source in [
            "fn run() { wrap!(<std::net::TcpStream>::connect(addr)); }",
            "use std::net::TcpStream as Stream; fn run() { wrap!(<Stream>::connect(addr)); }",
        ] {
            assert_eq!(
                violations(source).unwrap(),
                vec![Violation {
                    line: 1,
                    capability: "outbound TCP"
                }]
            );
        }
        let clean = r#"
            // std::net::TcpStream::connect(addr)
            fn run() { let text = "std::net::TcpStream::connect(addr)"; format!("{}", text); }
        "#;
        assert!(violations(clean).unwrap().is_empty());
    }

    #[test]
    fn only_test_required_cfg_subtrees_are_skipped() {
        for cfg in ["test", "all(test, unix)", "any(test, all(test, unix))"] {
            let source = format!(
                "#[cfg({cfg})] mod tests {{ fn run() {{ std::net::TcpStream::connect(addr); }} }}"
            );
            assert!(violations(&source).unwrap().is_empty(), "{source}");
        }
        for cfg in [
            "unix",
            "any(test, unix)",
            "not(test)",
            "all(unix, not(test))",
        ] {
            let source =
                format!("#[cfg({cfg})] fn run() {{ std::net::TcpStream::connect(addr); }}");
            assert_eq!(
                violations(&source).unwrap(),
                vec![Violation {
                    line: 1,
                    capability: "outbound TCP"
                }],
                "{source}"
            );
        }
        assert!(
            violations("#![cfg(test)]\nfn run() { std::net::TcpStream::connect(addr); }")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_only_aliases_cannot_hide_production_imports() {
        let source = "use std::net::TcpStream as Stream; #[cfg(test)] type Stream = Fake; fn run() { Stream::connect(addr); }";
        assert_eq!(
            violations(source).unwrap(),
            vec![Violation {
                line: 1,
                capability: "outbound TCP"
            }]
        );
    }

    #[test]
    fn test_only_associated_items_and_blocks_are_skipped() {
        let source = "struct App; impl App { #[cfg(test)] fn run() { std::net::TcpStream::connect(addr); } } trait AppTrait { #[cfg(test)] fn run() { std::net::TcpStream::connect(addr); } } fn run() { #[cfg(test)] { std::net::TcpStream::connect(addr); } }";
        assert!(violations(source).unwrap().is_empty());
        for source in [
            "struct App; impl App { #[cfg(any(test, unix))] fn run() { std::net::TcpStream::connect(addr); } }",
            "trait AppTrait { #[cfg(any(test, unix))] fn run() { std::net::TcpStream::connect(addr); } }",
            "fn run() { #[cfg(any(test, unix))] { std::net::TcpStream::connect(addr); } }",
        ] {
            assert_eq!(
                violations(source).unwrap(),
                vec![Violation {
                    line: 1,
                    capability: "outbound TCP"
                }]
            );
        }
    }

    #[test]
    fn alias_cycles_do_not_hide_network_alternatives() {
        assert!(
            violations("type A = B; type B = A; fn run() { A::connect(addr); }")
                .unwrap()
                .is_empty()
        );
        for source in [
            "type Stream = Loop; type Loop = Stream; use std::net::TcpStream as Stream; fn run() { Stream::connect(addr); }",
            "use std::net as std; fn run() { std::TcpStream::connect(addr); }",
        ] {
            assert_eq!(
                violations(source).unwrap(),
                vec![Violation {
                    line: 1,
                    capability: "outbound TCP"
                }],
                "{source}"
            );
        }
    }

    #[test]
    fn crate_and_imported_macro_names_do_not_recurse() {
        let source = "use anyhow::{Context as _, Result, anyhow, bail}; fn run() -> Result<()> { bail!(\"failure\"); }";
        assert!(violations(source).unwrap().is_empty());
    }

    #[test]
    fn qualified_same_name_imports_are_terminal() {
        for source in [
            "use super::secret; fn run() { secret::open(); }",
            "use super::{Method, Event}; fn run(method: Method, event: Event) {}",
            "use self::local::Stream; fn run() { Stream::connect(addr); }",
            "use crate::local::Stream; fn run() { Stream::connect(addr); }",
        ] {
            assert!(violations(source).unwrap().is_empty(), "{source}");
        }
        assert_eq!(
            violations("type Conn = std::net::TcpStream; fn run() { self::Conn::connect(addr); }")
                .unwrap(),
            vec![Violation {
                line: 1,
                capability: "outbound TCP"
            }]
        );
    }

    #[test]
    fn file_local_parent_qualifiers_resolve_network_aliases_and_allow_ipc() {
        for source in [
            "use std::net::TcpStream as Stream; mod child { fn run() { super::Stream::connect(addr); } }",
            "use std::net::TcpStream as Stream; mod child { mod nested { fn run() { super::super::Stream::connect(addr); } } }",
            "use std::net::TcpStream as Stream; fn run() { crate::Stream::connect(addr); }",
        ] {
            assert_eq!(
                violations(source).unwrap(),
                vec![Violation {
                    line: 1,
                    capability: "outbound TCP"
                }],
                "{source}"
            );
        }
        let ipc = "use std::os::unix::net::UnixStream as Stream; mod child { fn run() { super::Stream::connect(path); } }";
        assert!(violations(ipc).unwrap().is_empty());
    }

    fn fixture(root: &Path) -> Value {
        let packages: Vec<_> = ["dudes-hook", "shared", "dev-only"].into_iter().map(|name| {
            let src = root.join(name).join("src");
            fs::create_dir_all(&src).unwrap();
            fs::write(src.join("lib.rs"), "fn allowed() {}").unwrap();
            json!({"id": name, "name": name, "manifest_path": root.join(name).join("Cargo.toml")})
        }).collect();
        json!({
            "workspace_root": root,
            "workspace_members": ["dudes-hook", "shared", "dev-only"],
            "packages": packages,
            "resolve": {"nodes": [
                {"id": "dudes-hook", "deps": [
                    {"pkg": "shared", "dep_kinds": [{"kind": null}]},
                    {"pkg": "dev-only", "dep_kinds": [{"kind": "dev"}]}
                ]},
                {"id": "shared", "deps": []},
                {"id": "dev-only", "deps": []}
            ]}
        })
    }

    #[test]
    fn source_check_reads_transitive_workspace_src_and_excludes_dev_only_members() {
        let temp = tempfile::tempdir().unwrap();
        let metadata = fixture(temp.path());
        let graph = Graph::from_value(&metadata).unwrap();
        assert_eq!(
            source_paths(&metadata, &graph).unwrap(),
            vec![
                temp.path().join("dudes-hook/src/lib.rs"),
                temp.path().join("shared/src/lib.rs")
            ]
        );
        fs::write(
            temp.path().join("dev-only/src/lib.rs"),
            "fn run() { std::net::TcpStream::connect(addr); }",
        )
        .unwrap();
        check(&metadata, &graph).unwrap();
        fs::write(
            temp.path().join("shared/src/lib.rs"),
            "fn run() { std::net::TcpStream::connect(addr); }",
        )
        .unwrap();
        assert_eq!(
            check(&metadata, &graph).unwrap_err().to_string(),
            "1 outbound-network source violation(s)"
        );
    }

    #[test]
    fn source_check_has_no_test_filename_exemption_and_fails_on_unreadable_source() {
        let temp = tempfile::tempdir().unwrap();
        let metadata = fixture(temp.path());
        let graph = Graph::from_value(&metadata).unwrap();
        fs::write(
            temp.path().join("shared/src/tests.rs"),
            "fn run() { std::net::TcpStream::connect(addr); }",
        )
        .unwrap();
        assert!(check(&metadata, &graph).is_err());
        fs::remove_dir_all(temp.path().join("shared/src")).unwrap();
        assert!(source_paths(&metadata, &graph).is_err());
    }
}
