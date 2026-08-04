//! User-facing project import-graph reports.

use std::fs;
use std::io::{self, Write};

use anyhow::{Context, Result};

use crate::cli::{GraphArgs, GraphFormat};
use crate::core::atomic_file::replace as atomic_write;
use crate::core::json_output::{self, schema};
use crate::project::import_graph::ImportGraph;

use super::AppContext;

pub(super) fn graph(context: &AppContext, args: GraphArgs) -> Result<i32> {
    let project = context.project()?;
    let graph = ImportGraph::inspect(&project, &args.roots, args.external)?;
    let bytes = match args.format {
        GraphFormat::Json => json_output::render(schema::GRAPH, &graph)?,
        GraphFormat::Text => render_text(&graph).into_bytes(),
        GraphFormat::Dot => render_dot(&graph).into_bytes(),
    };
    if let Some(path) = args.output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        atomic_write(&path, &bytes)?;
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .context("failed to write import graph")?;
    }
    Ok(if graph.cycles.is_empty() { 0 } else { 1 })
}

fn render_text(graph: &ImportGraph) -> String {
    let mut output = format!(
        "{} modules, {} edges, {} cycle{}\n",
        graph.nodes.len(),
        graph.edges.len(),
        graph.cycles.len(),
        if graph.cycles.len() == 1 { "" } else { "s" }
    );
    for node in &graph.nodes {
        output.push_str(&format!(
            "{}\t{}\tdirect={} external={} transitive={}\n",
            node.module,
            node.path,
            node.project_imports,
            node.external_imports,
            node.transitive_project_imports
        ));
        for import in &node.imports {
            output.push_str(&format!("  -> {import}\n"));
        }
    }
    for cycle in &graph.cycles {
        output.push_str(&format!("cycle: {}\n", cycle.join(" -> ")));
    }
    output
}

fn render_dot(graph: &ImportGraph) -> String {
    let mut output = String::from("digraph lean_imports {\n  rankdir=LR;\n");
    for node in &graph.nodes {
        output.push_str(&format!(
            "  \"{}\" [label=\"{}\\n{}\"];\n",
            dot_escape(&node.module),
            dot_escape(&node.module),
            dot_escape(&node.path)
        ));
    }
    for edge in &graph.edges {
        if edge.external {
            output.push_str(&format!(
                "  \"{}\" [shape=box,style=dashed];\n",
                dot_escape(&edge.to)
            ));
        }
        output.push_str(&format!(
            "  \"{}\" -> \"{}\"{};\n",
            dot_escape(&edge.from),
            dot_escape(&edge.to),
            if edge.external { " [style=dashed]" } else { "" }
        ));
    }
    output.push_str("}\n");
    output
}

fn dot_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
