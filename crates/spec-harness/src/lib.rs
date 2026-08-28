//! Checking harness for spec documents: markdown files whose hand-written
//! prose surrounds generated regions, with mermaid diagrams pre-rendered
//! to SVG. Each spec-checked state machine's test module supplies the
//! domain content (rendered tables, the cargo aliases that regenerate
//! them); this crate holds the document mechanics they share: guarded
//! region lookup and replacement, the staleness asserts, the regenerate
//! writer, and the mmdc render pipeline with its source-hash pin.
//!
//! The conformance sweeps stay in each machine's test module. The spec
//! and the implementation are independent encodings of the transition
//! decision, and this crate interprets documents, never transitions, so
//! sharing it collapses nothing.

use std::path::Path;

/// A spec document and the places its generated artifacts live: the
/// markdown source, the directory of pre-rendered SVGs, and the two
/// cargo aliases the failure messages point at.
pub struct SpecDoc {
    /// The markdown source.
    pub path: &'static str,
    /// The directory holding the pre-rendered SVGs.
    pub diagrams_dir: &'static str,
    /// The cargo alias that regenerates the generated blocks.
    pub update_alias: &'static str,
    /// The cargo alias that re-renders the diagrams.
    pub render_alias: &'static str,
}

impl SpecDoc {
    fn read(&self) -> String {
        std::fs::read_to_string(self.path)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", self.path))
    }

    /// The byte range between `name`'s generated markers, exclusive of
    /// both.
    fn generated_region(&self, text: &str, name: &str) -> (usize, usize) {
        let begin = format!("<!-- BEGIN GENERATED: {name} -->\n");
        let end_marker = format!("<!-- END GENERATED: {name} -->");
        let start = text
            .find(&begin)
            .unwrap_or_else(|| panic!("{} has no {begin:?} marker", self.path))
            + begin.len();
        let end = text[start..]
            .find(&end_marker)
            .unwrap_or_else(|| panic!("{} has no {end_marker:?} marker", self.path))
            + start;
        // A second copy of the block would be neither checked nor
        // regenerated, so refuse the ambiguity.
        assert!(
            text[end..].find(&begin).is_none(),
            "{} has more than one {name} block",
            self.path
        );
        (start, end)
    }

    /// Holds every named block in the document equal to its fresh
    /// render, which is what keeps the document's claim of being
    /// generated true.
    pub fn assert_blocks_current(&self, blocks: &[(&str, String)]) {
        let text = self.read();
        for (name, content) in blocks {
            let (start, end) = self.generated_region(&text, name);
            assert!(
                text[start..end] == **content,
                "the {name} block in {} disagrees with the spec. \
                 Expected:\n\n{content}\nRun `cargo {}` to regenerate \
                 every block, and review the diff.",
                self.path,
                self.update_alias
            );
        }
    }

    /// Replaces every named block with its fresh render and writes the
    /// document back. Callers keep this behind an ignored test so a
    /// plain test run never writes to the source tree.
    pub fn regenerate(&self, blocks: &[(&str, String)]) {
        let mut text = self.read();
        for (name, content) in blocks {
            let (start, end) = self.generated_region(&text, name);
            text.replace_range(start..end, content);
        }
        std::fs::write(self.path, text)
            .unwrap_or_else(|error| panic!("could not write {}: {error}", self.path));
        eprintln!("rewrote the generated blocks in {}", self.path);
    }

    fn svg_path(&self, name: &str) -> String {
        format!("{}/{name}.svg", self.diagrams_dir)
    }

    /// Each rendered SVG pins the hash of the mermaid source it was
    /// rendered from, so editing a diagram without re-rendering fails
    /// here, and CI needs no mermaid toolchain to detect the drift.
    pub fn assert_diagrams_current(&self) {
        for (name, source) in mermaid_regions(&self.read()) {
            let path = self.svg_path(&name);
            let svg = std::fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!(
                    "could not read {path}: {error}; run `cargo {}`",
                    self.render_alias
                )
            });
            let expected = format!("<!-- mermaid-fnv1a: {:016x} -->", fnv1a(&source));
            assert!(
                svg.starts_with(&expected),
                "{name}: the rendered SVG is stale; run `cargo {}`",
                self.render_alias
            );
        }
    }

    /// Renders each mermaid region to `<diagrams_dir>/<name>.svg` and
    /// pins the source hash. Callers keep this behind an ignored test
    /// so a plain test run never needs the mermaid CLI.
    ///
    /// Determinism boundary: with the id fixes below, re-rendering an
    /// unchanged source is byte-stable on one machine, and CI never
    /// compares SVG bytes at all (the check pins the source hash), so
    /// environments can differ without breaking anything. Across
    /// machines, Chromium versions and font fallbacks still move text
    /// metrics, so a re-render on different hardware may churn measured
    /// coordinates; that churn is confined to commits that edit a
    /// diagram. Byte-stability across machines would need a pinned
    /// render container, which this mechanism deliberately omits.
    pub fn render_diagrams(&self) {
        for (name, source) in mermaid_regions(&self.read()) {
            let body: String = source
                .lines()
                .filter(|line| !line.trim_start().starts_with("```"))
                .collect::<Vec<_>>()
                .join("\n");
            let input = std::env::temp_dir().join(format!("{name}.mmd"));
            let rendered = std::env::temp_dir().join(format!("{name}.svg"));
            let config = std::env::temp_dir().join(format!("{name}.mermaid.json"));
            std::fs::write(&input, &body).expect("write the mermaid source");
            // Deterministic ids, seeded by the diagram name: mermaid
            // otherwise embeds a random token in every render, and a
            // re-render with an unchanged source would dirty the tree.
            //
            // htmlLabels off, everywhere: HTML labels sit in foreignObject
            // boxes that clip at their measured edge, and SVG text
            // overflows visibly instead. State diagrams read the flowchart
            // key for edge labels, so all three keys are needed.
            //
            // The font stack must carry no quotes: rustdoc's markdown
            // pipeline applies smart punctuation to the text inside the
            // inlined SVG's style block, so a quoted "trebuchet ms"
            // arrives as a curly-quoted unknown font and the browser falls
            // back to a wider face than the one mmdc measured, clipping
            // every label. Quote-free names survive, and mmdc then
            // measures the same face the browser renders.
            std::fs::write(
                &config,
                format!(
                    r#"{{"deterministicIds": true, "deterministicIDSeed": "{name}",
                         "htmlLabels": false, "state": {{"htmlLabels": false}},
                         "flowchart": {{"htmlLabels": false}},
                         "themeVariables": {{"fontFamily": "verdana, arial, sans-serif"}}}}"#
                ),
            )
            .expect("write the mermaid config");

            let status = std::process::Command::new("mmdc")
                .arg("-i")
                .arg(&input)
                .arg("-o")
                .arg(&rendered)
                .arg("-c")
                .arg(&config)
                .status()
                .expect("mmdc should be installed: npm i -g @mermaid-js/mermaid-cli");
            assert!(status.success(), "mmdc failed for {name}");

            let svg = std::fs::read_to_string(&rendered).expect("read the rendered svg");
            // mmdc can prepend an XML declaration; rustdoc wants raw <svg>.
            let svg = svg.trim_start_matches(|c| c != '<');
            let svg = if svg.starts_with("<?xml") {
                &svg[svg.find("?>").map(|i| i + 2).unwrap_or(0)..]
            } else {
                svg
            };
            // The deterministicIds config misses the internal ids of
            // composite states, which carry a fresh random token on every
            // render; normalize them so an unchanged source re-renders
            // byte-identically and never dirties the tree.
            let svg = normalize_state_ids(svg);
            let path = self.svg_path(&name);
            std::fs::create_dir_all(self.diagrams_dir).expect("create the diagrams directory");
            std::fs::write(
                Path::new(&path),
                format!(
                    "<!-- mermaid-fnv1a: {:016x} -->\n{}",
                    fnv1a(&source),
                    svg.trim_start()
                ),
            )
            .expect("write the pinned svg");
            eprintln!("rendered {path}");
        }
    }
}

/// Every mermaid region in the document: its name and its fenced
/// source. The source in the document is the diagram; each crate's
/// build script swaps the region for its pre-rendered SVG in the
/// rustdoc variant, so GitHub renders the fence and rustdoc renders
/// the drawing.
fn mermaid_regions(text: &str) -> Vec<(String, String)> {
    let mut sources = vec![];
    let mut rest = text;
    while let Some(start) = rest.find("<!-- BEGIN MERMAID: ") {
        let name_start = start + "<!-- BEGIN MERMAID: ".len();
        let name_end = rest[name_start..]
            .find(" -->")
            .expect("a mermaid marker name")
            + name_start;
        let name = rest[name_start..name_end].to_string();
        let body_start = name_end + " -->".len();
        let end_marker = format!("<!-- END MERMAID: {name} -->");
        let end = rest.find(&end_marker).expect("a closing mermaid marker");
        sources.push((name, rest[body_start..end].trim().to_string()));
        rest = &rest[end + end_marker.len()..];
    }
    sources
}

/// FNV-1a, implemented locally: the pin must be stable across Rust
/// releases, which std's `DefaultHasher` does not promise.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Replaces each distinct random token after `state-id-` with its
/// first-occurrence index, rewriting every reference to it the same
/// way, so internal id links inside the SVG stay consistent.
fn normalize_state_ids(svg: &str) -> String {
    const MARKER: &str = "state-id-";
    let mut tokens: Vec<String> = vec![];
    let mut output = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(found) = rest.find(MARKER) {
        let after = found + MARKER.len();
        let token_len = rest[after..]
            .bytes()
            .take_while(u8::is_ascii_alphanumeric)
            .count();
        let token = rest[after..after + token_len].to_string();
        let index = match tokens.iter().position(|seen| *seen == token) {
            Some(index) => index,
            None => {
                tokens.push(token);
                tokens.len() - 1
            }
        };
        output.push_str(&rest[..after]);
        output.push_str(&format!("d{index}"));
        rest = &rest[after + token_len..];
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> SpecDoc {
        SpecDoc {
            path: "fixture.md",
            diagrams_dir: "fixture-diagrams",
            update_alias: "fixture-update",
            render_alias: "fixture-render",
        }
    }

    const FIXTURE: &str = "prose\n\
        <!-- BEGIN GENERATED: table -->\n\
        old\n\
        <!-- END GENERATED: table -->\n\
        more prose\n\
        <!-- BEGIN MERMAID: flow -->\n\
        ```mermaid\n\
        stateDiagram-v2\n\
        ```\n\
        <!-- END MERMAID: flow -->\n";

    #[test]
    fn a_generated_region_spans_exactly_its_interior() {
        let (start, end) = doc().generated_region(FIXTURE, "table");
        assert_eq!(&FIXTURE[start..end], "old\n");
    }

    #[test]
    #[should_panic(expected = "has no")]
    fn a_missing_marker_is_refused() {
        doc().generated_region(FIXTURE, "absent");
    }

    #[test]
    #[should_panic(expected = "more than one")]
    fn a_duplicated_block_is_refused() {
        let doubled = format!(
            "{FIXTURE}<!-- BEGIN GENERATED: table -->\n\
             again\n\
             <!-- END GENERATED: table -->\n"
        );
        doc().generated_region(&doubled, "table");
    }

    #[test]
    fn mermaid_regions_walk_every_region() {
        assert_eq!(
            mermaid_regions(FIXTURE),
            vec![(
                "flow".to_string(),
                "```mermaid\nstateDiagram-v2\n```".to_string()
            )]
        );
    }

    #[test]
    fn state_ids_normalize_to_first_occurrence_indices() {
        assert_eq!(
            normalize_state_ids("state-id-abc12 state-id-zz9 state-id-abc12"),
            "state-id-d0 state-id-d1 state-id-d0"
        );
    }

    /// The published FNV-1a test vectors; a change to the hash strands
    /// every rendered diagram as stale.
    #[test]
    fn the_hash_pin_is_stable() {
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c);
    }
}
