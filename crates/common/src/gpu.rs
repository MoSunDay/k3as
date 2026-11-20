//! GPU vendor resource constants (T7.2, Q61).
//!
//! One place for the whole-GPU extended-resource names and the
//! vendor-specific visibility env vars, so kubelet publication, scheduler
//! accounting, API validation, and controllers never drift apart.
//! Whole-device only: fractional GPUs / MIG / time-slicing are explicitly
//! out of scope for the first slice (Q61).

/// NVIDIA whole-GPU extended resource (requests/limits must be integers).
pub const GPU_RESOURCE_NVIDIA: &str = "nvidia.com/gpu";
/// AMD whole-GPU extended resource (requests/limits must be integers).
pub const GPU_RESOURCE_AMD: &str = "amd.com/gpu";
/// Every GPU vendor resource this platform understands (Q61).
pub const GPU_RESOURCES: [&str; 2] = [GPU_RESOURCE_NVIDIA, GPU_RESOURCE_AMD];

/// Env var injected for NVIDIA device visibility (value: selected indices).
pub const NVIDIA_VISIBLE_ENV: &str = "CUDA_VISIBLE_DEVICES";
/// Env var injected for AMD device visibility (value: selected indices).
pub const AMD_VISIBLE_ENV: &str = "HIP_VISIBLE_DEVICES";

/// The visibility env var for a GPU resource name, if it is a known vendor.
pub fn visible_env(resource: &str) -> Option<&'static str> {
    match resource {
        GPU_RESOURCE_NVIDIA => Some(NVIDIA_VISIBLE_ENV),
        GPU_RESOURCE_AMD => Some(AMD_VISIBLE_ENV),
        _ => None,
    }
}

/// True when `resource` is one of the known whole-GPU resources.
pub fn is_gpu_resource(resource: &str) -> bool {
    GPU_RESOURCES.contains(&resource)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_vendors_to_env_vars() {
        assert_eq!(visible_env(GPU_RESOURCE_NVIDIA), Some(NVIDIA_VISIBLE_ENV));
        assert_eq!(visible_env(GPU_RESOURCE_AMD), Some(AMD_VISIBLE_ENV));
        assert_eq!(visible_env("cpu"), None);
        assert!(is_gpu_resource("nvidia.com/gpu"));
        assert!(!is_gpu_resource("memory"));
    }
}
