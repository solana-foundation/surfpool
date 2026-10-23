use surfpool_types::TransactionMetadata;

#[test]
fn unavailable_loaded_account_size_is_not_a_zero_measurement() {
    let value = serde_json::to_value(TransactionMetadata::default()).unwrap();
    assert!(value["loaded_accounts_data_size"].is_null());
}

#[test]
fn old_metadata_without_a_measurement_stays_unavailable() {
    let mut value = serde_json::to_value(TransactionMetadata::default()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("loaded_accounts_data_size");
    let metadata: TransactionMetadata = serde_json::from_value(value).unwrap();
    let roundtrip = serde_json::to_value(metadata).unwrap();
    assert!(roundtrip["loaded_accounts_data_size"].is_null());
}

#[test]
fn measured_loaded_account_size_roundtrips_without_changing_the_number() {
    for bytes in [0, 357, 20_000] {
        let mut value = serde_json::to_value(TransactionMetadata::default()).unwrap();
        value["loaded_accounts_data_size"] = bytes.into();
        let metadata: TransactionMetadata = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(metadata).unwrap(), value);
    }
}
