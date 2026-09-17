use std::{env, fs, path::Path};

/// Produces the rustdoc variant of `slot-lifecycle.md`: each mermaid
/// region is replaced by its pre-rendered SVG from
/// `src/surfnet/diagrams/`, so rustdoc shows the drawing while the
/// source file keeps the editable fence, which GitHub and editors
/// render natively. The test `the_diagrams_match_their_renderings`
/// holds the SVGs to their sources; this script only splices.
fn main() {
    println!("cargo:rerun-if-changed=src/surfnet/slot-lifecycle.md");
    println!("cargo:rerun-if-changed=src/surfnet/diagrams");

    let source = fs::read_to_string("src/surfnet/slot-lifecycle.md")
        .expect("slot-lifecycle.md should exist");

    let mut output = String::new();
    let mut rest = source.as_str();
    loop {
        let Some(start) = rest.find("<!-- BEGIN MERMAID: ") else {
            output.push_str(rest);
            break;
        };
        let name_start = start + "<!-- BEGIN MERMAID: ".len();
        let name_end = rest[name_start..]
            .find(" -->")
            .expect("a mermaid marker name")
            + name_start;
        let name = &rest[name_start..name_end];
        let end_marker = format!("<!-- END MERMAID: {name} -->");
        let end = rest
            .find(&end_marker)
            .unwrap_or_else(|| panic!("no closing marker for mermaid region {name}"))
            + end_marker.len();

        output.push_str(&rest[..start]);
        let svg_path = format!("src/surfnet/diagrams/{name}.svg");
        let svg = fs::read_to_string(&svg_path)
            .unwrap_or_else(|error| panic!("could not read {svg_path}: {error}"));
        output.push_str(&svg);
        rest = &rest[end..];
    }

    let out = Path::new(&env::var("OUT_DIR").expect("OUT_DIR")).join("slot-lifecycle.rustdoc.md");
    fs::write(out, output).expect("write the rustdoc variant");
}
