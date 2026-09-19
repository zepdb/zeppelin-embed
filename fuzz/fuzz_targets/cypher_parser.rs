#![no_main]
use libfuzzer_sys::fuzz_target;
use zeppelin_embed_cypher::{Budget, CompileLimits, compile_with, parse_bytes};

fuzz_target!(|bytes: &[u8]| {
    if let Ok(text) = std::str::from_utf8(bytes) {
        let _ = compile_with(
            text,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                for column in bound.columns() {
                    assert!(
                        bound
                            .expressions()
                            .get(column.expression.0 as usize)
                            .is_some()
                    );
                }
                for call in bound.calls() {
                    assert!(bound.syntax().node(call.syntax).is_some());
                }
                Ok(())
            },
        );
    }
    let mut budget = Budget::new(24 * 1024 * 1024, None);
    if let Ok(ast) = parse_bytes(bytes, CompileLimits::default(), &mut budget) {
        assert_eq!(ast.source().as_bytes(), bytes);
        assert!(ast.node(ast.root()).is_some());
        assert!(ast.nodes().len() <= 4096);
        ast.visit(&mut budget, |_, node| {
            assert!(ast.source().get(node.span.start..node.span.end).is_some());
            for child in node.children() {
                assert!(ast.node(*child).is_some());
            }
            Ok(())
        })
        .expect("bounded AST visitor");
    }
});
