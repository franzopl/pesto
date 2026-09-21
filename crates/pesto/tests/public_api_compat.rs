//! Compile-time coverage for legacy public module paths.

#[test]
fn legacy_child_module_paths_remain_public() {
    let _ = pesto::config::parse::default_config_path;
    let _ = std::mem::size_of::<pesto::config::types::Config>();
    let _ = pesto::config::validation::validate_groups;

    let _ = std::mem::size_of::<pesto::memory::budget::Stage>();
    let _ = std::mem::size_of::<pesto::memory::ceiling::Ceiling>();
    let _ = std::mem::size_of::<pesto::memory::cgroup::CgroupMemory>();
    let _ = std::mem::size_of::<pesto::memory::pressure::Pressure>();

    let _ = pesto::yenc::decode::decode_part;
    let _ = pesto::yenc::scalar::encode_scalar;

    #[cfg(target_arch = "aarch64")]
    let _ = pesto::yenc::aarch64::encode;

    #[cfg(target_arch = "x86_64")]
    let _ = pesto::yenc::x86::encode;
}
