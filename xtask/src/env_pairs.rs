//! `cargo xtask env-pairs` — reject raw environment name/value representations.
//!
//! `dudes_providers::launch::EnvVar` is the representation that keeps an
//! environment value out of a derived `Debug`. This gate checks the source form that can bypass
//! that type. It parses Rust items instead of opening a line window at a declaration: rustfmt line
//! wrapping, restricted visibility, tuple terminators and `where` clauses therefore do not change
//! which item owns a field.
//!
//! The table covers every workspace library under `crates/` and the two binaries that hold an
//! environment, `dudes` and `duded`. The hook shim and `xtask` hold no environment-bearing source
//! type and remain outside the applicable set. Those exclusions are visible in
//! [`applicable_manifest`], and [`coverage_issues`] makes the table and the applicable member set
//! equal in both directions.
//!
//! This is a source guard, not a proof about run-time values. It recognises the string and byte
//! representations in [`Resolver::is_stringish`], environment-bearing identifiers in
//! [`environment_name`] — which splits an identifier at separators, camel-case humps and digit
//! boundaries, so a numbered spelling such as `env2` still carries the word `env` while a word
//! that merely starts with those letters, such as `envelope`, does not —
//! and type declarations or free, impl and trait method parameters visible to `syn`. It resolves
//! an unambiguous type alias in the same source file when a bare single-segment path names it,
//! binding each supplied argument in the scope that wrote it and each default in the scope of the
//! parameters before it, so nested aliases may reuse a parameter name. A qualified path such as
//! `other::Pair` or `<T as Trait>::Pair` is never taken for the local `Pair`, so an alias reached
//! only through `crate::` or `self::` is outside its evidence. A recursive alias, or a chain
//! deeper than [`MAX_ALIAS_DEPTH`] aliases, is reported as an error rather than resolved. A macro
//! can generate a type the parser cannot see; same-named aliases in different scopes and an
//! opaque, neutrally named wrapper remain outside its evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, bail};
use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Fields, FnArg, GenericArgument, ItemEnum, ItemFn, ItemStruct, ItemType, PathArguments,
    ReturnType, TraitItemFn, Type,
};

use crate::metadata::{self, Package};
use crate::workspace::Workspace;

const EXEMPT: &str = "purity-ok:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rule {
    member: &'static str,
}

/// Every workspace library under `crates/` and both binaries that hold an environment.
const RULES: &[Rule] = &[
    Rule { member: "duded" },
    Rule { member: "dudes" },
    Rule {
        member: "dudes-config",
    },
    Rule {
        member: "dudes-host",
    },
    Rule {
        member: "dudes-journal",
    },
    Rule {
        member: "dudes-policy",
    },
    Rule {
        member: "dudes-pty",
    },
    Rule {
        member: "dudes-proto",
    },
    Rule {
        member: "dudes-providers",
    },
    Rule {
        member: "dudes-rpc",
    },
    Rule {
        member: "dudes-supervisor",
    },
    Rule {
        member: "dudes-types",
    },
    Rule {
        member: "dudes-win",
    },
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Violation {
    line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CoverageIssue {
    Uncovered(String),
    Stale(String),
}

/// Parse one Rust source file and return every offending source line.
fn violations(source: &str) -> Result<Vec<Violation>> {
    let syntax = syn::parse_file(source).context("source is not valid Rust syntax")?;
    let mut aliases = AliasCollector::default();
    aliases.visit_file(&syntax);
    let mut collector = Collector {
        source,
        resolver: Resolver {
            aliases: &aliases.items,
            limit: None,
        },
        violations: Vec::new(),
    };
    collector.visit_file(&syntax);
    match collector.resolver.limit {
        Some(Limit::Recursive { alias, line }) => bail!(
            "type alias `{alias}` (line {line}) refers to itself; the checker does not resolve a recursive alias"
        ),
        Some(Limit::Depth { alias, line }) => bail!(
            "type alias `{alias}` (line {line}) nests deeper than {MAX_ALIAS_DEPTH} aliases; the checker stops there"
        ),
        None => {}
    }
    collector.violations.sort();
    collector.violations.dedup();
    Ok(collector.violations)
}

type Aliases<'ast> = BTreeMap<String, Vec<&'ast ItemType>>;

/// Alias chains deeper than this are reported instead of resolved. The frame chain is the only
/// recursion the resolver adds beyond a type's own syntax, so this bounds the stack it can use.
const MAX_ALIAS_DEPTH: usize = 32;

/// A type parameter's argument together with the scope that argument was written in. Reading the
/// argument back in that scope, never in the alias being entered, is what lets nested aliases
/// reuse a parameter name without the resolver chasing `T` to a type that itself mentions `T`.
#[derive(Clone)]
struct Bound<'ast> {
    ty: &'ast Type,
    scope: Scope<'ast>,
}

/// One alias body being resolved: its bound parameters and the scope it was entered from. The
/// parent chain is what the recursion and depth guards read; parameter lookup never climbs it,
/// because an alias body can name only its own parameters.
struct Frame<'ast> {
    alias: String,
    params: BTreeMap<String, Bound<'ast>>,
    parent: Scope<'ast>,
}

/// `None` is the root scope: an item's own declaration, where no alias parameter is bound.
type Scope<'ast> = Option<Rc<Frame<'ast>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Limit {
    Recursive { alias: String, line: usize },
    Depth { alias: String, line: usize },
}

#[derive(Default)]
struct AliasCollector<'ast> {
    items: Aliases<'ast>,
}

impl<'ast> Visit<'ast> for AliasCollector<'ast> {
    fn visit_item_type(&mut self, item: &'ast ItemType) {
        self.items
            .entry(item.ident.to_string())
            .or_default()
            .push(item);
        visit::visit_item_type(self, item);
    }
}

struct Collector<'source, 'ast> {
    source: &'source str,
    resolver: Resolver<'source, 'ast>,
    violations: Vec<Violation>,
}

impl<'ast> Collector<'_, 'ast> {
    fn record(&mut self, span: Span) {
        let line = span.start().line;
        let exempt = self
            .source
            .lines()
            .nth(line.saturating_sub(1))
            .is_some_and(|text| text.contains(EXEMPT));
        if !exempt {
            self.violations.push(Violation { line });
        }
    }

    fn check_fields(&mut self, owner: &str, fields: &'ast Fields) {
        let owner_is_environment = environment_name(owner);
        for field in fields {
            let field_is_environment = field
                .ident
                .as_ref()
                .is_some_and(|ident| environment_name(&ident.to_string()));
            if self.resolver.is_raw_pair(&field.ty, &None)
                && (owner_is_environment || field_is_environment)
            {
                self.record(field.ty.span());
            }
        }

        if owner_is_environment {
            let string_fields: Vec<&Type> = fields
                .iter()
                .filter(|field| self.resolver.is_stringish(&field.ty, &None))
                .map(|field| &field.ty)
                .collect();
            if string_fields.len() >= 2 {
                self.record(string_fields[0].span());
            }
        }
    }

    fn check_signature(
        &mut self,
        name: &str,
        inputs: &'ast syn::punctuated::Punctuated<FnArg, syn::token::Comma>,
        output: &'ast ReturnType,
    ) {
        for input in inputs {
            let FnArg::Typed(argument) = input else {
                continue;
            };
            let named = match argument.pat.as_ref() {
                syn::Pat::Ident(ident) => environment_name(&ident.ident.to_string()),
                _ => false,
            };
            if named && self.resolver.is_raw_pair(&argument.ty, &None) {
                self.record(argument.ty.span());
            }
        }
        if environment_name(name)
            && let ReturnType::Type(_, ty) = output
            && self.resolver.is_raw_pair(ty, &None)
        {
            self.record(ty.span());
        }
    }
}

impl<'ast> Visit<'ast> for Collector<'_, 'ast> {
    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        self.check_fields(&item.ident.to_string(), &item.fields);
        visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast ItemEnum) {
        let owner = item.ident.to_string();
        for variant in &item.variants {
            let name = if environment_name(&variant.ident.to_string()) {
                variant.ident.to_string()
            } else {
                owner.clone()
            };
            self.check_fields(&name, &variant.fields);
        }
        visit::visit_item_enum(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast ItemType) {
        if environment_name(&item.ident.to_string()) && self.resolver.alias_is_raw_pair(item) {
            self.record(item.ty.span());
        }
        visit::visit_item_type(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.check_signature(
            &item.sig.ident.to_string(),
            &item.sig.inputs,
            &item.sig.output,
        );
        visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.check_signature(
            &item.sig.ident.to_string(),
            &item.sig.inputs,
            &item.sig.output,
        );
        visit::visit_impl_item_fn(self, item);
    }

    fn visit_trait_item_fn(&mut self, item: &'ast TraitItemFn) {
        self.check_signature(
            &item.sig.ident.to_string(),
            &item.sig.inputs,
            &item.sig.output,
        );
        visit::visit_trait_item_fn(self, item);
    }
}

fn environment_name(name: &str) -> bool {
    identifier_words(name)
        .iter()
        .any(|word| matches!(word.as_str(), "env" | "environment" | "vars" | "variables"))
}

/// Split an identifier into lower-case words at separators, camel-case humps and digit
/// boundaries, so `env2`, `Env2` and `daemon_env` all yield the word `env`.
fn identifier_words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    for (index, ch) in chars.iter().copied().enumerate() {
        if ch == '_' || ch == '-' {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            continue;
        }
        // A run of digits is its own word: `env2` is an environment binding numbered 2, not a
        // binding named `env2`, and reading it as one let the numbered spellings through.
        let word_ends_in_digit = word
            .chars()
            .next_back()
            .is_some_and(|last| last.is_ascii_digit());
        if !word.is_empty() && ch.is_ascii_digit() != word_ends_in_digit {
            words.push(std::mem::take(&mut word));
        }
        let previous_is_lower = index > 0 && chars[index - 1].is_ascii_lowercase();
        let next_is_lower = chars.get(index + 1).is_some_and(char::is_ascii_lowercase);
        if ch.is_ascii_uppercase() && !word.is_empty() && (previous_is_lower || next_is_lower) {
            words.push(std::mem::take(&mut word));
        }
        word.push(ch.to_ascii_lowercase());
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Type classification over one file's aliases. `limit` records the first recursion or depth
/// refusal; [`violations`] turns it into an error so a chain the resolver gave up on is never
/// reported as clean.
struct Resolver<'a, 'ast> {
    aliases: &'a Aliases<'ast>,
    limit: Option<Limit>,
}

impl<'ast> Resolver<'_, 'ast> {
    fn alias_is_raw_pair(&mut self, alias: &'ast ItemType) -> bool {
        let Some(frame) = bind(alias, &[], &None) else {
            return false;
        };
        self.is_raw_pair(&alias.ty, &Some(frame))
    }

    fn is_raw_pair(&mut self, ty: &'ast Type, scope: &Scope<'ast>) -> bool {
        match ty {
            Type::Reference(reference) => self.is_raw_pair(&reference.elem, scope),
            Type::Paren(paren) => self.is_raw_pair(&paren.elem, scope),
            Type::Group(group) => self.is_raw_pair(&group.elem, scope),
            Type::Slice(slice) => self.is_raw_pair(&slice.elem, scope),
            Type::Array(array) => self.is_raw_pair(&array.elem, scope),
            Type::Tuple(tuple) => {
                tuple.elems.len() == 2
                    && tuple
                        .elems
                        .iter()
                        .all(|element| self.is_stringish(element, scope))
            }
            Type::Path(path) => {
                let Some(segment) = path.path.segments.last() else {
                    return false;
                };
                if let Some(bound) = parameter(path, scope) {
                    return self.is_raw_pair(bound.ty, &bound.scope);
                }
                let arguments = type_arguments(&segment.arguments);
                match segment.ident.to_string().as_str() {
                    "HashMap" | "BTreeMap" | "IndexMap" => {
                        arguments.len() >= 2
                            && self.is_stringish(arguments[0], scope)
                            && self.is_stringish(arguments[1], scope)
                    }
                    "Vec" | "VecDeque" | "SmallVec" | "Box" | "Rc" | "Arc" | "Option" => arguments
                        .first()
                        .is_some_and(|inner| self.is_raw_pair(inner, scope)),
                    _ => self.resolve(path, scope, Self::is_raw_pair),
                }
            }
            _ => false,
        }
    }

    fn is_stringish(&mut self, ty: &'ast Type, scope: &Scope<'ast>) -> bool {
        match ty {
            Type::Reference(reference) => self.is_stringish(&reference.elem, scope),
            Type::Paren(paren) => self.is_stringish(&paren.elem, scope),
            Type::Group(group) => self.is_stringish(&group.elem, scope),
            Type::Slice(slice) => is_u8(&slice.elem),
            Type::Array(array) => is_u8(&array.elem),
            Type::Path(path) => {
                let Some(segment) = path.path.segments.last() else {
                    return false;
                };
                if let Some(bound) = parameter(path, scope) {
                    return self.is_stringish(bound.ty, &bound.scope);
                }
                let arguments = type_arguments(&segment.arguments);
                match segment.ident.to_string().as_str() {
                    "String" | "OsString" | "str" | "OsStr" => arguments.is_empty(),
                    "Box" | "Cow" => arguments
                        .last()
                        .is_some_and(|inner| self.is_stringish(inner, scope)),
                    "Vec" => arguments.first().is_some_and(|inner| is_u8(inner)),
                    _ => self.resolve(path, scope, Self::is_stringish),
                }
            }
            _ => false,
        }
    }

    /// Enter the unambiguous local alias a bare path names and classify its body with
    /// `predicate`. An alias already on the frame chain, or a chain at [`MAX_ALIAS_DEPTH`], is
    /// refused and recorded in `limit`.
    fn resolve(
        &mut self,
        path: &'ast syn::TypePath,
        scope: &Scope<'ast>,
        predicate: fn(&mut Self, &'ast Type, &Scope<'ast>) -> bool,
    ) -> bool {
        if !is_unqualified(path) {
            return false;
        }
        let segment = &path.path.segments[0];
        let name = segment.ident.to_string();
        let Some(&[alias]) = self.aliases.get(&name).map(Vec::as_slice) else {
            return false;
        };
        let line = alias.ident.span().start().line;
        if chain(scope).any(|frame| frame.alias == name) {
            self.limit
                .get_or_insert(Limit::Recursive { alias: name, line });
            return false;
        }
        if chain(scope).count() >= MAX_ALIAS_DEPTH {
            self.limit.get_or_insert(Limit::Depth { alias: name, line });
            return false;
        }
        let Some(frame) = bind(alias, &type_arguments(&segment.arguments), scope) else {
            return false;
        };
        predicate(self, &alias.ty, &Some(frame))
    }
}

/// A bare single-segment path such as `Pair`. `other::Pair`, `::Pair` and `<T as Trait>::Pair`
/// name something this file's aliases and parameters cannot be, so they are never resolved here.
fn is_unqualified(path: &syn::TypePath) -> bool {
    path.qself.is_none() && path.path.leading_colon.is_none() && path.path.segments.len() == 1
}

/// The binding a bare path names in the current frame, if it is one of that alias's parameters.
fn parameter<'s, 'ast>(path: &syn::TypePath, scope: &'s Scope<'ast>) -> Option<&'s Bound<'ast>> {
    if !is_unqualified(path) {
        return None;
    }
    scope
        .as_deref()?
        .params
        .get(&path.path.segments[0].ident.to_string())
}

fn chain<'s, 'ast>(scope: &'s Scope<'ast>) -> impl Iterator<Item = &'s Frame<'ast>> {
    std::iter::successors(scope.as_deref(), |frame| frame.parent.as_deref())
}

/// Bind `alias`'s type parameters for one use. A supplied argument is read back in `caller`,
/// the scope that wrote it; a default is read back among the parameters declared before it, as
/// the language scopes it. No frame ever binds a name to a type read back in that same frame.
fn bind<'ast>(
    alias: &'ast ItemType,
    supplied: &[&'ast Type],
    caller: &Scope<'ast>,
) -> Option<Rc<Frame<'ast>>> {
    let parameters: Vec<&'ast syn::TypeParam> = alias.generics.type_params().collect();
    if supplied.len() > parameters.len() {
        return None;
    }
    let name = alias.ident.to_string();
    let mut params: BTreeMap<String, Bound<'ast>> = BTreeMap::new();
    for (index, parameter) in parameters.into_iter().enumerate() {
        let bound = match supplied.get(index) {
            Some(&ty) => Bound {
                ty,
                scope: caller.clone(),
            },
            None => Bound {
                ty: &parameter.default.as_ref()?.1,
                scope: Some(Rc::new(Frame {
                    alias: name.clone(),
                    params: params.clone(),
                    parent: caller.clone(),
                })),
            },
        };
        params.insert(parameter.ident.to_string(), bound);
    }
    Some(Rc::new(Frame {
        alias: name,
        params,
        parent: caller.clone(),
    }))
}

fn type_arguments(arguments: &PathArguments) -> Vec<&Type> {
    let PathArguments::AngleBracketed(arguments) = arguments else {
        return Vec::new();
    };
    arguments
        .args
        .iter()
        .filter_map(|argument| match argument {
            GenericArgument::Type(ty) => Some(ty),
            _ => None,
        })
        .collect()
}

fn is_u8(ty: &Type) -> bool {
    matches!(ty, Type::Path(path) if path.qself.is_none() && path.path.is_ident("u8"))
}

fn applicable(package: &Package) -> bool {
    applicable_manifest(&package.manifest)
}

fn applicable_manifest(manifest: &str) -> bool {
    (manifest.starts_with("crates/") && manifest.ends_with("/Cargo.toml"))
        || matches!(manifest, "bins/dudes/Cargo.toml" | "bins/duded/Cargo.toml")
}

fn coverage_issues<'a>(
    members: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<CoverageIssue> {
    let workspace: BTreeSet<&str> = members
        .into_iter()
        .filter(|(_, manifest)| applicable_manifest(manifest))
        .map(|(name, _)| name)
        .collect();
    let rules: BTreeSet<&str> = RULES.iter().map(|rule| rule.member).collect();
    workspace
        .difference(&rules)
        .map(|name| CoverageIssue::Uncovered((*name).to_owned()))
        .chain(
            rules
                .difference(&workspace)
                .map(|name| CoverageIssue::Stale((*name).to_owned())),
        )
        .collect()
}

fn sources(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)
        .with_context(|| format!("cannot read source directory {}", dir.display()))?
    {
        let path = entry
            .with_context(|| format!("cannot read an entry under {}", dir.display()))?
            .path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// What one walk of a checkout found: the coverage errors between [`RULES`] and the workspace's
/// applicable `crates/` members and the two environment-holding binaries, and each covered
/// member's offending `path:line` locations.
///
/// The command and the test that keeps it live under `cargo nextest` both read this, so the
/// gate cannot pass in one and fail in the other.
struct Report {
    coverage: Vec<CoverageIssue>,
    members: Vec<(&'static str, Vec<String>)>,
}

/// Run the whole check over the checkout rooted at `root`: resolve the workspace members, compare
/// them with [`RULES`], then parse every covered member's `src` tree.
///
/// A coverage error returns before any source is read — a table that does not describe this
/// workspace cannot say which sources the walk should have covered.
fn scan(root: &Path) -> Result<Report> {
    let metadata = metadata::cargo_metadata()?;
    let members = metadata::members(&metadata)?;
    let coverage = coverage_issues(
        members
            .iter()
            .map(|package| (package.name.as_str(), package.manifest.as_str())),
    );
    if !coverage.is_empty() {
        return Ok(Report {
            coverage,
            members: Vec::new(),
        });
    }

    let by_name: BTreeMap<&str, &Package> = members
        .iter()
        .filter(|package| applicable(package))
        .map(|package| (package.name.as_str(), package))
        .collect();
    let mut scanned = Vec::new();
    for rule in RULES {
        let package = by_name
            .get(rule.member)
            .with_context(|| format!("covered workspace member `{}` disappeared", rule.member))?;
        let manifest = root.join(&package.manifest);
        let src = manifest
            .parent()
            .context("workspace member manifest has no parent")?
            .join("src");
        let mut member_broken = Vec::new();
        for path in sources(&src)? {
            let source = fs::read_to_string(&path)
                .with_context(|| format!("cannot read Rust source {}", path.display()))?;
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("{} is outside the workspace", path.display()))?;
            for violation in violations(&source)
                .with_context(|| format!("cannot check {}", relative.display()))?
            {
                member_broken.push(format!("{}:{}", relative.display(), violation.line));
            }
        }
        scanned.push((rule.member, member_broken));
    }
    Ok(Report {
        coverage,
        members: scanned,
    })
}

pub(crate) fn run(workspace: &Workspace) -> Result<()> {
    let report = scan(workspace.root())?;
    for issue in &report.coverage {
        match issue {
            CoverageIssue::Uncovered(member) => println!(
                "env-pairs: {member:<18} UNCOVERED (applicable workspace member with no row in RULES)"
            ),
            CoverageIssue::Stale(member) => {
                println!(
                    "env-pairs: {member:<18} STALE (RULES row has no applicable workspace member)"
                );
            }
        }
    }
    if !report.coverage.is_empty() {
        bail!(
            "{} env-pairs coverage error(s); RULES and applicable workspace members must be set-equal",
            report.coverage.len()
        );
    }

    let mut broken = 0usize;
    for (member, locations) in &report.members {
        if locations.is_empty() {
            println!("env-pairs: {member:<18} ok");
        } else {
            for location in locations {
                println!(
                    "env-pairs: {location} VIOLATION raw environment name/value representation; use EnvVar"
                );
            }
            broken += locations.len();
        }
    }
    if broken > 0 {
        bail!("{broken} raw environment representation(s)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn environment_field_with_an_os_string_pair_is_rejected() {
        let source = "pub struct Hire {\n    pub environment: Vec<(OsString, OsString)>,\n}\n";
        assert_eq!(violations(source).unwrap(), [Violation { line: 2 }]);
    }

    #[test]
    fn a_second_derived_debug_env_var_does_not_share_the_canonical_exemption() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("crates/dudes-providers/src/launch.rs");
        let mut source = fs::read_to_string(path).unwrap();
        let before = source.lines().count();
        source.push_str(
            "\n#[derive(Debug)]\npub struct EnvVar {\n    name: OsString,\n    value: OsString,\n}\n",
        );
        assert_eq!(
            violations(&source).unwrap(),
            [Violation { line: before + 4 }]
        );
    }

    #[test]
    fn a_defaulted_environment_alias_and_its_use_are_rejected() {
        let source = "pub type EnvironmentPair<T = String> = (T, T);\npub struct Hire {\n    pub env: Vec<EnvironmentPair>,\n}\n";
        assert_eq!(
            violations(source).unwrap(),
            [Violation { line: 1 }, Violation { line: 3 }]
        );
    }

    #[test]
    fn a_trait_method_environment_parameter_is_rejected() {
        let source = "pub trait Adapter {\n    fn launch(&self, env: &[(String, String)]);\n}\n";
        assert_eq!(violations(source).unwrap(), [Violation { line: 2 }]);
    }

    #[test]
    fn nested_aliases_reusing_a_parameter_name_terminate_and_resolve() {
        let source = "pub type A<T> = T;\npub type B<T> = A<Vec<T>>;\npub type C<T> = B<Option<T>>;\npub struct Hire {\n    pub env: C<(String, String)>,\n    pub vars: B<(OsString, OsString)>,\n    pub environment: C<Vec<EnvVar>>,\n}\n";
        assert_eq!(
            violations(source).unwrap(),
            [Violation { line: 5 }, Violation { line: 6 }]
        );
    }

    #[test]
    fn direct_and_multilevel_aliases_still_detect_raw_pairs() {
        let source = "pub type Pair = (OsString, OsString);\npub type Pairs = Vec<Pair>;\npub type Bundle = Pairs;\npub struct Hire {\n    pub env: Pair,\n    pub environment: Pairs,\n    pub vars: Bundle,\n}\n";
        assert_eq!(
            violations(source).unwrap(),
            [
                Violation { line: 5 },
                Violation { line: 6 },
                Violation { line: 7 }
            ]
        );
    }

    #[test]
    fn a_qualified_path_is_not_resolved_as_the_local_alias() {
        let cases = [
            ("module path", "other::Pair"),
            ("crate-root path", "::Pair"),
            ("qualified self path", "<Self>::Pair"),
            ("trait-qualified path", "<Self as Adapter>::Pair"),
            ("alias as a prefix", "Pair::Names"),
        ];
        for (shape, path) in cases {
            let source = format!(
                "pub type Pair = (String, String);\npub struct Hire {{\n    pub env: Vec<{path}>,\n}}\n"
            );
            assert_eq!(
                violations(&source).unwrap(),
                [],
                "{shape} was resolved as the local alias"
            );
        }
    }

    #[test]
    fn a_local_raw_pair_alias_still_fails() {
        let source =
            "pub type Pair = (String, String);\npub struct Hire {\n    pub env: Vec<Pair>,\n}\n";
        assert_eq!(violations(source).unwrap(), [Violation { line: 3 }]);
    }

    #[test]
    fn recursive_and_mutually_recursive_aliases_terminate_with_a_report() {
        let cases = [
            (
                "self-recursive",
                "pub type Loop = Vec<Loop>;\npub struct Hire {\n    pub env: Loop,\n}\n",
                "`Loop`",
            ),
            (
                "mutually recursive",
                "pub type Ping = Vec<Pong>;\npub type Pong = Vec<Ping>;\npub struct Hire {\n    pub env: Ping,\n}\n",
                "`Ping`",
            ),
            (
                "generic self-recursive",
                "pub type Grow<T> = Grow<Vec<T>>;\npub struct Hire {\n    pub env: Grow<String>,\n}\n",
                "`Grow`",
            ),
            (
                "environment-named self-recursive",
                "pub type EnvLoop = Vec<EnvLoop>;\n",
                "`EnvLoop`",
            ),
        ];
        for (shape, source, alias) in cases {
            let error = violations(source).unwrap_err().to_string();
            assert!(
                error.contains("refers to itself") && error.contains(alias),
                "{shape}: {error}"
            );
        }
    }

    #[test]
    fn a_default_binds_in_the_scope_of_the_parameters_before_it() {
        let source = "pub type P<T, U = T> = (T, U);\npub type D<T = (String, String)> = Vec<T>;\npub type E<T = String> = P<T>;\npub struct Hire {\n    pub env: P<String>,\n    pub vars: D,\n    pub environment: E,\n}\n";
        assert_eq!(
            violations(source).unwrap(),
            [
                Violation { line: 5 },
                Violation { line: 6 },
                Violation { line: 7 }
            ]
        );
    }

    #[test]
    fn an_alias_chain_deeper_than_the_limit_is_reported() {
        fn chain(levels: usize) -> String {
            (0..levels)
                .map(|index| {
                    if index + 1 == levels {
                        format!("pub type A{index} = (String, String);\n")
                    } else {
                        format!("pub type A{index} = Vec<A{}>;\n", index + 1)
                    }
                })
                .chain(["pub struct Hire {\n    pub env: A0,\n}\n".to_owned()])
                .collect()
        }
        assert_eq!(
            violations(&chain(MAX_ALIAS_DEPTH)).unwrap(),
            [Violation {
                line: MAX_ALIAS_DEPTH + 2
            }]
        );
        let error = violations(&chain(MAX_ALIAS_DEPTH + 1))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("deeper than") && error.contains(&format!("`A{MAX_ALIAS_DEPTH}`")),
            "{error}"
        );
    }

    #[test]
    fn every_measured_raw_environment_shape_is_rejected() {
        let cases = [
            (
                "rustfmt-wrapped field",
                "pub struct Hire {\n    pub this_field_name_is_long_enough_that_rustfmt_places_its_environment_env:\n        Vec<(OsString, OsString)>,\n}\n",
            ),
            (
                "named intermediate",
                "pub struct EnvPair {\n    pub name: String,\n    pub value: String,\n}\n\npub struct Hire {\n    pub env: Vec<EnvPair>,\n}\n",
            ),
            (
                "public tuple fields",
                "pub struct DaemonEnv(pub OsString, pub OsString);\n",
            ),
            (
                "boxed strings",
                "pub struct Hire { pub env: Vec<(Box<str>, Box<str>)> }\n",
            ),
            (
                "borrowed cow strings",
                "pub struct Hire { pub env: Vec<(Cow<'static, str>, Cow<'static, str>)> }\n",
            ),
            (
                "byte value",
                "pub struct Hire { pub env: Vec<(OsString, Vec<u8>)> }\n",
            ),
            (
                "vars field",
                "pub struct Hire { pub vars: Vec<(OsString, OsString)> }\n",
            ),
            (
                "super visibility",
                "pub(super) struct DaemonEnv { pub pairs: Vec<(OsString, OsString)> }\n",
            ),
            (
                "in-path visibility",
                "pub(in crate::adapter) struct DaemonEnv { pub pairs: Vec<(OsString, OsString)> }\n",
            ),
            (
                "map",
                "pub struct Hire { pub environment: HashMap<OsString, OsString> }\n",
            ),
            (
                "borrowed slice",
                "pub struct Ctx<'a> { pub daemon_env: &'a [(&'a str, &'a str)] }\n",
            ),
            (
                "function parameter",
                "pub fn install(env: &[(String, String)]) {}\n",
            ),
            (
                "qualified paths",
                "pub struct Hire { pub env: Vec<(std::ffi::OsString, std::ffi::OsString)> }\n",
            ),
            (
                "type alias",
                "pub type EnvironmentPairs = Vec<(OsString, OsString)>;\n",
            ),
        ];
        for (shape, source) in cases {
            assert!(
                !violations(source).unwrap().is_empty(),
                "{shape} escaped the rule"
            );
        }
    }

    #[test]
    fn item_boundaries_and_safe_types_do_not_raise_false_positives() {
        let cases = [
            (
                "rustfmt-split tuple struct",
                "pub struct DaemonEnv(\n    Vec<EnvVar>,\n);\n\npub fn headers() -> Vec<(String, String)> { Vec::new() }\n",
            ),
            (
                "where-clause tuple struct",
                "pub struct DaemonEnv<T>(Vec<EnvVar>, T)\nwhere\n    T: Clone;\n\npub fn headers() -> Vec<(String, String)> { Vec::new() }\n",
            ),
            (
                "empty body",
                "pub struct DaemonEnv {}\n\npub fn headers() -> Vec<(String, String)> { Vec::new() }\n",
            ),
            (
                "marked canonical redacting newtype",
                "pub struct EnvVar { name: OsString, value: OsString } // purity-ok: canonical redacting EnvVar\n",
            ),
            (
                "non-environment return",
                "pub fn headers() -> Vec<(String, String)> { Vec::new() }\n",
            ),
            (
                "path and string pair",
                "pub struct ManagedInstall { pub env_files: Vec<(PathBuf, String)> }\n",
            ),
            (
                "envelope is not environment",
                "pub struct StreamEnvelope { pub fields: Vec<(String, String)> }\n",
            ),
        ];
        for (shape, source) in cases {
            assert_eq!(
                violations(source).unwrap(),
                [],
                "{shape} raised a false positive"
            );
        }
    }

    #[test]
    fn an_exemption_opens_only_the_line_that_carries_it() {
        let source = "pub struct Hire {\n    pub env: Vec<(String, String)>, // purity-ok: names only\n    pub vars: Vec<(OsString, OsString)>,\n}\n";
        assert_eq!(violations(source).unwrap(), [Violation { line: 3 }]);
    }

    #[test]
    fn invalid_rust_fails_closed() {
        let error = violations("pub struct EnvPair {").unwrap_err().to_string();
        assert!(error.contains("valid Rust syntax"), "{error}");
    }

    #[test]
    fn the_source_walk_reaches_nested_rust_files_only() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("hit.rs"), "pub struct Environment;").unwrap();
        fs::write(root.path().join("ignored.txt"), "not Rust").unwrap();
        assert_eq!(sources(root.path()).unwrap(), [nested.join("hit.rs")]);
    }

    #[test]
    fn rule_rows_and_applicable_workspace_members_are_set_equal_both_ways() {
        let clean: Vec<(&str, &str)> = RULES
            .iter()
            .map(|rule| match rule.member {
                "duded" => (rule.member, "bins/duded/Cargo.toml"),
                "dudes" => (rule.member, "bins/dudes/Cargo.toml"),
                _ => (rule.member, "crates/member/Cargo.toml"),
            })
            .collect();
        assert_eq!(coverage_issues(clean.iter().copied()), []);

        let mut with_new_member = clean.clone();
        with_new_member.push(("dudes-new", "crates/dudes-new/Cargo.toml"));
        assert_eq!(
            coverage_issues(with_new_member),
            [CoverageIssue::Uncovered("dudes-new".to_owned())]
        );

        let without_existing: Vec<(&str, &str)> = clean
            .into_iter()
            .filter(|(name, _)| *name != "dudes-host")
            .collect();
        assert_eq!(
            coverage_issues(without_existing),
            [CoverageIssue::Stale("dudes-host".to_owned())]
        );
    }

    #[test]
    fn a_numbered_environment_identifier_is_rejected() {
        let cases = [
            (
                "numbered field",
                "pub struct Hire {\n    pub env2: Vec<(OsString, OsString)>,\n}\n",
            ),
            (
                "numbered tuple struct",
                "pub struct Env2(pub OsString, pub OsString);\n",
            ),
            (
                "numbered owner",
                "pub struct DaemonEnv2 {\n    pub pairs: Vec<(OsString, OsString)>,\n}\n",
            ),
        ];
        for (shape, source) in cases {
            assert!(
                !violations(source).unwrap().is_empty(),
                "{shape} escaped the rule"
            );
        }
        assert_eq!(identifier_words("env2"), ["env", "2"]);
        assert_eq!(identifier_words("Env2Var"), ["env", "2", "var"]);
    }

    /// The control for [`a_numbered_environment_identifier_is_rejected`]: ending a word at a digit
    /// must not make an identifier that merely contains digits environmental.
    #[test]
    fn a_digit_does_not_make_an_unrelated_identifier_environmental() {
        let source = "pub struct Utf8Header {\n    pub sha256_fields: Vec<(String, String)>,\n    pub envelope2: Vec<(String, String)>,\n}\n";
        assert_eq!(violations(source).unwrap(), []);
    }

    /// The check the command performs, over the tree it will perform it on: the workspace members
    /// `cargo metadata` reports, filtered the way [`applicable`] filters them, compared with
    /// [`RULES`], and every covered source tree parsed. This is what makes the gate live under
    /// `cargo nextest` before the subcommand is wired into CI.
    #[test]
    fn the_workspace_as_it_stands_holds_no_raw_environment_pair() {
        let report = scan(&workspace_root()).unwrap_or_else(|err| panic!("{err:#}"));
        assert_eq!(
            report.coverage,
            [],
            "RULES and applicable workspace members must be set-equal"
        );
        let broken: Vec<&String> = report
            .members
            .iter()
            .flat_map(|(_, locations)| locations)
            .collect();
        assert_eq!(broken, Vec::<&String>::new());
        // Positive control: the walk read real files rather than an empty member list.
        assert_eq!(report.members.len(), RULES.len(), "{:?}", report.members);
    }

    /// The workspace root holding this checkout's `xtask`, as [`crate::naming`]'s corpus test
    /// resolves it.
    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask/ has a parent")
            .to_owned()
    }

    #[test]
    fn environment_holding_binaries_are_applicable_and_other_tools_are_not() {
        assert!(applicable_manifest("bins/dudes/Cargo.toml"));
        assert!(applicable_manifest("bins/duded/Cargo.toml"));
        assert!(!applicable_manifest("bins/dudes-hook/Cargo.toml"));
        assert!(!applicable_manifest("xtask/Cargo.toml"));
    }
}
