//! Append-only shared status names; this does not enable graph operations.
use zeppelin_embed_ffi::ze_error_code_name;

const GRAPH_ERRORS: &[(i32, &str)] = &[
    (35, "ZE_ERR_STORE_KIND"),
    (36, "ZE_ERR_FORMAT_VERSION"),
    (37, "ZE_ERR_QUERY_SYNTAX"),
    (38, "ZE_ERR_QUERY_UNSUPPORTED"),
    (39, "ZE_ERR_PARAMETER"),
    (40, "ZE_ERR_TYPE"),
    (41, "ZE_ERR_SCOPE"),
    (42, "ZE_ERR_KEY_CONFLICT"),
    (43, "ZE_ERR_INCARNATION_CONFLICT"),
    (44, "ZE_ERR_DELETION_REVISION_CONFLICT"),
    (45, "ZE_ERR_ENDPOINT"),
    (46, "ZE_ERR_DELETED_ENTITY"),
    (47, "ZE_ERR_ARITHMETIC_DOMAIN"),
    (48, "ZE_ERR_ARITHMETIC_OVERFLOW"),
    (49, "ZE_ERR_DIVISION_BY_ZERO"),
    (50, "ZE_ERR_INDETERMINATE_COMMIT"),
    (51, "ZE_ERR_REVISION_OVERFLOW"),
    (52, "ZE_ERR_GENERATION_OVERFLOW"),
    (53, "ZE_ERR_DUPLICATE_TARGET"),
    (54, "ZE_ERR_IDENTITY_OVERFLOW"),
    (55, "ZE_ERR_REVISION_CONFLICT"),
    (56, "ZE_ERR_FORMAT_TOO_NEW"),
    (57, "ZE_ERR_CASCADE_CYCLE"),
    (58, "ZE_ERR_LEGACY_GRAPH_DIRECTORY"),
    (59, "ZE_ERR_GRAPH_UNSUPPORTED_BUILD"),
    (60, "ZE_ERR_GRAPH_EPOCH_TRANSITION"),
    (61, "ZE_ERR_GRAPH_DISABLED"),
];

#[test]
fn appended_graph_error_names_are_distinct_and_stable() {
    for &(code, expected) in GRAPH_ERRORS {
        let actual = unsafe { std::ffi::CStr::from_ptr(ze_error_code_name(code)) };
        assert_eq!(actual.to_str().unwrap(), expected);
    }
    for unknown in [-1, 62, i32::MAX] {
        let actual = unsafe { std::ffi::CStr::from_ptr(ze_error_code_name(unknown)) };
        assert_eq!(actual.to_str().unwrap(), "ZE_ERR_UNKNOWN");
    }
}
