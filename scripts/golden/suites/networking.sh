# Q32: the router rides openrusty with Ingress (networking.k8s.io/v1) as the
# first-class ingress resource. G31 pins the group discovery byte-stable;
# route compilation + real traffic is exercised by scripts/service-traffic-e2e.sh
# (ST1-ST6) and scripts/flannel-e2e.sh (F1-F5), not here.
check_body G31 "/apis/networking.k8s.io/v1" "$GOLDEN/discovery-networking-v1.json"
