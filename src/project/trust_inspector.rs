//! Generated Lean inspection for declaration axioms and safety.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::project::import_graph;

pub const OUTPUT_PREFIX: &str = "LEV_TRUST_JSON:";

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct TrustRecord {
    pub name: String,
    pub found: bool,
    #[serde(rename = "unsafe")]
    pub is_unsafe: bool,
    pub axioms: Vec<String>,
}

pub fn render(modules: &[String], declarations: &[String]) -> Result<String> {
    let mut source = String::new();
    for module in modules {
        let module = import_graph::canonical_lean_name(module)?;
        source.push_str("import ");
        source.push_str(&module);
        source.push('\n');
    }
    source.push_str("import Lean\n\n");
    source.push_str(
        r#"open Lean Elab Command

private def levTrustRecord (name : Name) : CommandElabM Json := do
  let environment <- getEnv
  match environment.find? name with
  | none =>
      return Json.mkObj [
        ("name", Json.str name.toString),
        ("found", toJson false),
        ("unsafe", toJson false),
        ("axioms", toJson (#[] : Array String))
      ]
  | some info =>
      let axioms := (← collectAxioms name).qsort Name.lt
      return Json.mkObj [
        ("name", Json.str name.toString),
        ("found", toJson true),
        ("unsafe", toJson info.isUnsafe),
        ("axioms", toJson (axioms.map Name.toString))
      ]

private def levTrustDeclarationNames : Array String := #[
"#,
    );
    for (index, declaration) in declarations.iter().enumerate() {
        source.push_str("  ");
        source.push_str(
            &serde_json::to_string(declaration)
                .context("failed to encode a declaration name for Lean inspection")?,
        );
        if index + 1 < declarations.len() {
            source.push(',');
        }
        source.push('\n');
    }
    source.push_str(
        r#"]

elab "lev_emit_trust_report" : command => do
  let records <- levTrustDeclarationNames.mapM fun declaration =>
    levTrustRecord declaration.toName
  for record in records do
    liftIO <| IO.println ("LEV_TRUST_JSON:" ++ record.compress)

lev_emit_trust_report
"#,
    );
    Ok(source)
}

pub fn parse_output(bytes: &[u8]) -> Result<(Vec<TrustRecord>, Vec<String>)> {
    let mut records = Vec::new();
    let mut other_lines = Vec::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        if let Some(value) = line.strip_prefix(OUTPUT_PREFIX) {
            records.push(
                serde_json::from_str(value)
                    .with_context(|| format!("invalid Lean trust record: {value}"))?,
            );
        } else if !line.trim().is_empty() {
            other_lines.push(line.to_owned());
        }
    }
    Ok((records, other_lines))
}

#[cfg(test)]
mod tests {
    use super::{TrustRecord, parse_output, render};

    #[test]
    fn renders_imports_and_escaped_declaration_names() {
        let source = render(
            &["Root".to_owned()],
            &["Root.answer".to_owned(), "Root.name\"quoted".to_owned()],
        )
        .unwrap();
        assert!(source.starts_with("import Root\nimport Lean\n"));
        assert!(source.contains("\"Root.name\\\"quoted\""));
        assert!(source.contains("collectAxioms name"));
        assert!(source.contains("let records <- levTrustDeclarationNames.mapM"));
        assert!(source.ends_with("lev_emit_trust_report\n"));
    }

    #[test]
    fn parses_only_prefixed_protocol_records() {
        let (records, other) = parse_output(
            br#"ordinary output
LEV_TRUST_JSON:{"name":"Root.answer","found":true,"unsafe":false,"axioms":["propext"]}
"#,
        )
        .unwrap();
        assert_eq!(
            records,
            [TrustRecord {
                name: "Root.answer".to_owned(),
                found: true,
                is_unsafe: false,
                axioms: vec!["propext".to_owned()],
            }]
        );
        assert_eq!(other, ["ordinary output"]);
    }
}
