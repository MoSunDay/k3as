//! The accept-no-op-warn flag set (Q9 matrix, Table C).
//!
//! Every k3s `server`/`agent` flag that is not accept-wired (Table A) and
//! not a fatal conflict (Table B) lives here. These are accepted, stripped
//! from argv *before* clap, and logged once per process at WARN. Keeping
//! them as a data table (rather than clap fields) keeps the clap surface at
//! the 17 wired flags (R1).

/// One accept-no-op-warn flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoopFlag {
    pub long: &'static str,
    pub short: Option<char>,
    /// `true` if the flag takes a value (string/int/slice/duration);
    /// `false` for bare bool flags.
    pub takes_value: bool,
}

/// The frozen accept-no-op-warn set (Table C, exhaustive).
#[rustfmt::skip]
pub const NOOP_FLAGS: &[NoopFlag] = &[
    // C.1 Logging / verbosity
    nf("v", None, true),
    nf("vmodule", None, true),
    nf("log", Some('l'), true),
    nf("alsologtostderr", None, false),
    // C.2 Listener / TLS / SAN
    // (`bind-address` + `https-listen-port` are wired in T1.2a; kept here are
    // the remaining k3s listener/TLS flags that remain no-op for v1.)
    nf("supervisor-port", None, true),
    nf("apiserver-port", None, true),
    nf("apiserver-bind-address", None, true),
    nf("advertise-address", None, true),
    nf("advertise-port", None, true),
    nf("tls-san", None, true),
    nf("tls-san-security", None, false),
    // C.3 Cluster networking
    nf("cluster-cidr", None, true),
    nf("service-cidr", None, true),
    nf("service-node-port-range", None, true),
    nf("cluster-dns", None, true),
    nf("cluster-domain", None, true),
    nf("flannel-backend", None, true),
    nf("flannel-ipv6-masq", None, false),
    nf("flannel-external-ip", None, false),
    nf("egress-selector-mode", None, true),
    nf("servicelb-namespace", None, true),
    nf("flannel-iface", None, true),
    nf("flannel-conf", None, true),
    nf("flannel-cni-conf", None, true),
    // C.4 Kubeconfig / client bootstrap
    nf("write-kubeconfig", Some('o'), true),
    nf("write-kubeconfig-mode", None, true),
    nf("write-kubeconfig-group", None, true),
    nf("token-file", None, true),
    nf("agent-token", None, true),
    nf("agent-token-file", None, true),
    // C.5 Cluster lifecycle / snapshots
    nf("cluster-reset", None, false),
    nf("cluster-reset-restore-path", None, true),
    nf("helm-job-image", None, true),
    nf("etcd-expose-metrics", None, false),
    nf("etcd-disable-snapshots", None, false),
    nf("etcd-snapshot-name", None, true),
    nf("etcd-snapshot-schedule-cron", None, true),
    nf("etcd-snapshot-reconcile-interval", None, true),
    nf("etcd-snapshot-retention", None, true),
    nf("etcd-snapshot-dir", None, true),
    nf("etcd-snapshot-compress", None, false),
    nf("etcd-s3", None, false),
    nf("etcd-s3-endpoint", None, true),
    nf("etcd-s3-endpoint-ca", None, true),
    nf("etcd-s3-skip-ssl-verify", None, false),
    nf("etcd-s3-access-key", None, true),
    nf("etcd-s3-secret-key", None, true),
    nf("etcd-s3-session-token", None, true),
    nf("etcd-s3-bucket", None, true),
    nf("etcd-s3-bucket-lookup-type", None, true),
    nf("etcd-s3-region", None, true),
    nf("etcd-s3-folder", None, true),
    nf("etcd-s3-retention", None, true),
    nf("etcd-s3-proxy", None, true),
    nf("etcd-s3-config-secret", None, true),
    nf("etcd-s3-insecure", None, false),
    nf("etcd-s3-timeout", None, true),
    // C.6 Datastore extras / kine
    nf("kine-tls", None, false),
    nf("datastore-cafile", None, true),
    nf("datastore-certfile", None, true),
    nf("datastore-keyfile", None, true),
    // C.7 Component arg passthrough
    nf("kube-apiserver-arg", None, true),
    nf("etcd-arg", None, true),
    nf("kube-controller-manager-arg", None, true),
    nf("kube-controller-arg", None, true),
    nf("kube-scheduler-arg", None, true),
    nf("kube-cloud-controller-manager-arg", None, true),
    nf("kube-cloud-controller-arg", None, true),
    nf("helm-controller-arg", None, true),
    nf("kubelet-arg", None, true),
    nf("kube-proxy-arg", None, true),
    // C.8 Storage / registry / images
    nf("default-local-storage-path", None, true),
    nf("embedded-registry", None, false),
    nf("supervisor-metrics", None, false),
    nf("system-default-registry", None, true),
    nf("pause-image", None, true),
    nf("private-registry", None, true),
    nf("disable-default-registry-endpoint", None, false),
    nf("airgap-extra-registry", None, true),
    // C.9 Node identity / kubelet
    nf("node-name", None, true),
    nf("with-node-id", None, false),
    nf("node-label", None, true),
    nf("node-taint", None, true),
    nf("node-ip", Some('i'), true),
    nf("node-external-ip", None, true),
    nf("node-internal-dns", None, true),
    nf("node-external-dns", None, true),
    nf("resolv-conf", None, true),
    nf("protect-kernel-defaults", None, false),
    // C.10 Container runtime
    nf("container-runtime-endpoint", None, true),
    nf("default-runtime", None, true),
    nf("image-service-endpoint", None, true),
    nf("snapshotter", None, true),
    nf("image-credential-provider-bin-dir", None, true),
    nf("image-credential-provider-config", None, true),
    nf("nonroot-devices", None, false),
    nf("docker", None, false),
    nf("disable-apiserver-lb", None, false),
    // C.11 Security / misc / experimental
    nf("secrets-encryption", None, false),
    nf("secrets-encryption-provider", None, true),
    nf("rootless", None, false),
    nf("selinux", None, false),
    nf("lb-server-port", None, true),
    nf("enable-pprof", None, false),
    nf("vpn-auth", None, true),
    nf("vpn-auth-file", None, true),
];

/// Const constructor (keeps the table `const`-friendly and readable).
const fn nf(long: &'static str, short: Option<char>, takes_value: bool) -> NoopFlag {
    NoopFlag {
        long,
        short,
        takes_value,
    }
}

/// Look up a no-op flag by its `--long` name.
pub fn find_long(name: &str) -> Option<&'static NoopFlag> {
    NOOP_FLAGS.iter().find(|f| f.long == name)
}

/// Look up a no-op flag by its `-x` short name.
pub fn find_short(c: char) -> Option<&'static NoopFlag> {
    NOOP_FLAGS.iter().find(|f| f.short == Some(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_expected_size() {
        // Table C enumerates ~108 distinct flag entries.
        assert!(NOOP_FLAGS.len() >= 100, "got {}", NOOP_FLAGS.len());
    }

    #[test]
    fn no_long_name_collides_with_wired() {
        // Wired flag long names (Table A) must NOT appear in the no-op set.
        let wired = [
            "data-dir",
            "debug",
            "config",
            "disable",
            "disable-etcd",
            "disable-apiserver",
            "disable-agent",
            "disable-controller-manager",
            "disable-scheduler",
            "disable-cloud-controller",
            "disable-kube-proxy",
            "disable-network-policy",
            "disable-helm-controller",
            "datastore-endpoint",
            "prefer-bundled-bin",
            "token",
            "server",
            "cluster-init",
            "bind-address",
            "https-listen-port",
        ];
        for w in wired {
            assert!(find_long(w).is_none(), "{w} must not be no-op");
        }
    }

    #[test]
    fn value_flags_take_value() {
        assert!(find_long("cluster-cidr").unwrap().takes_value);
        assert!(find_long("v").unwrap().takes_value);
        assert!(!find_long("rootless").unwrap().takes_value);
        assert!(!find_long("cluster-reset").unwrap().takes_value);
    }

    #[test]
    fn short_lookups() {
        assert_eq!(find_short('l').unwrap().long, "log");
        assert_eq!(find_short('o').unwrap().long, "write-kubeconfig");
        assert_eq!(find_short('i').unwrap().long, "node-ip");
        assert!(find_short('z').is_none());
    }
}
