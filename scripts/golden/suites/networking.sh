# Q32: the router rides openrusty with Ingress (networking.k8s.io/v1) as the
# first-class ingress resource. G31 pins the group discovery byte-stable.
# Route compilation is exercised in-process (router crate tests); the SERVED
# plane today is the Service nodePort plane (scripts/service-traffic-e2e.sh
# ST1-ST6, scripts/flannel-e2e.sh F1-F5). The compiled Ingress host-routing
# table has NO listener yet — T5.6 remaining scope, pinned executably by
# G70-G72 in suites/realtraffic.sh (Sprint 29).
check_body G31 "/apis/networking.k8s.io/v1" "$GOLDEN/discovery-networking-v1.json"
