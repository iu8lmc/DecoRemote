#!/usr/bin/env bash
# Installa il server DecoRemote come servizio systemd su un VPS Linux x86_64.
# Uso (come root):   curl -fsSL https://raw.githubusercontent.com/iu8lmc/DecoRemote/main/deploy/install.sh | sudo bash
# Variabili:  VERSION=v0.1.1  (release da scaricare)   PORT=21116
set -euo pipefail

VERSION="${VERSION:-v0.1.1}"
PORT="${PORT:-21116}"
REPO="iu8lmc/DecoRemote"
BASE="https://github.com/${REPO}/releases/download/${VERSION}"
RAW="https://raw.githubusercontent.com/${REPO}/main/deploy"

[ "$(id -u)" -eq 0 ] || { echo "Eseguire come root (sudo)." >&2; exit 1; }
[ "$(uname -m)" = "x86_64" ] || { echo "Architettura $(uname -m) non supportata: serve x86_64." >&2; exit 1; }
command -v systemctl >/dev/null || { echo "systemd non trovato." >&2; exit 1; }

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

echo ">> Scarico decoremote-server ${VERSION}"
curl -fsSL "${BASE}/decoremote-server-linux-x86_64" -o "$tmp/decoremote-server"
curl -fsSL "${BASE}/decoremote-server-linux-x86_64.sha256" -o "$tmp/sha256"
(cd "$tmp" && echo "$(cut -d' ' -f1 sha256)  decoremote-server" | sha256sum -c -)

install -m 0755 "$tmp/decoremote-server" /usr/local/bin/decoremote-server
curl -fsSL "${RAW}/decoremote-server.service" -o "$tmp/unit"
sed "s|0.0.0.0:21116|0.0.0.0:${PORT}|" "$tmp/unit" > /etc/systemd/system/decoremote-server.service

systemctl daemon-reload
systemctl enable --now decoremote-server
sleep 1
systemctl --no-pager --lines=5 status decoremote-server || true

# Firewall locale (se attivo). Quello del provider (security group) va aperto a mano.
P2=$((PORT + 1))
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  ufw allow "${PORT}/tcp"; ufw allow "${PORT}/udp"; ufw allow "${P2}/udp"
elif command -v firewall-cmd >/dev/null && firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --permanent --add-port="${PORT}/tcp" --add-port="${PORT}/udp" --add-port="${P2}/udp"
  firewall-cmd --reload
else
  echo "Nessun firewall locale attivo rilevato."
fi

echo
echo "Fatto. Aprire anche nel firewall del provider: TCP ${PORT}, UDP ${PORT}, UDP ${P2}."
echo "Nei client:  decoremote --server IP_PUBBLICO_DEL_VPS:${PORT}"
