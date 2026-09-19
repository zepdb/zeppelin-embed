#![no_main]
use libfuzzer_sys::fuzz_target;
use zeppelin_embed_cypher::{Budget, CompileLimits, parse_bytes};

fuzz_target!(|bytes: &[u8]| {
    let mut budget = Budget::new(24 * 1024 * 1024, None);
    if let Ok(ast) = parse_bytes(bytes, CompileLimits::default(), &mut budget) {
        assert_eq!(ast.source().as_bytes(), bytes);
        assert!(ast.node(ast.root()).is_some());
        assert!(ast.nodes().len() <= 4096);
        ast.visit(&mut budget, |_, node| {
            assert!(ast.source().get(node.span.start..node.span.end).is_some());
            for child in node.children() { assert!(ast.node(*child).is_some()); }
            Ok(())
        }).expect("bounded AST visitor");
    }
});
