# netguard

Checks wlo1 once a minute (IPv4 address, IPv4 default route, gateway ping, static IPv6 address, Thread route, WiFi association). After 3 consecutive failures it repairs, least invasive first: re-add IPv6 address/route with `ip`, then `networkctl renew`, then `networkctl reconfigure`, finally `systemctl restart systemd-networkd` (at most once per hour). A gateway-only outage and a lost WiFi association are only logged. Nothing happens while `/var/lib/heatd-failsafe/maintenance` exists. Journal: `journalctl -u heatd-netguard`.

    sudo install -d /var/lib/heatd-netguard
    sudo chmod +x /home/aapphh/MLSmartMatterHeat/deploy/netguard/heatd-netguard.sh
    sudo cp /home/aapphh/MLSmartMatterHeat/deploy/netguard/heatd-netguard.{service,timer} /etc/systemd/system/
    sudo systemctl daemon-reload
    sudo systemctl enable --now heatd-netguard.timer

Settings via `NETGUARD_*` environment variables (IFACE, GATEWAY, ADDR6, ROUTE6, VIA6, THRESHOLD, RESTART_INTERVAL), or via the optional gitignored file `deploy/netguard/netguard.env` next to the script (copy `netguard.env.example`; it is sourced first). ADDR6, ROUTE6 and VIA6 default to empty; while empty, the IPv6 address/route checks and repairs are skipped. Test: `bash deploy/netguard/test.sh`.
