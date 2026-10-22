# Server DecoRemote su VPS

Il server fa da rendezvous e relay; il contenuto delle sessioni resta cifrato end-to-end.
Servono un VPS Linux x86_64 con IP pubblico e queste porte aperte (anche nel firewall del provider):

| Porta | Uso |
|---|---|
| TCP 21116 | registrazione host, relay |
| UDP 21116 | scoperta dell'indirizzo pubblico, collegamento diretto |
| UDP 21117 | seconda porta per "Verifica rete" (tipo di NAT) |

## Con systemd (consigliato)

```bash
curl -fsSL https://raw.githubusercontent.com/iu8lmc/DecoRemote/main/deploy/install.sh | sudo bash
```

Scarica il binario statico dalla release (verificando lo SHA-256), crea il servizio `decoremote-server`
(utente temporaneo, sandbox systemd) e apre le porte in `ufw`/`firewalld` se attivi.
`VERSION=v0.1.0` e `PORT=21116` si possono cambiare con variabili d'ambiente.

```bash
systemctl status decoremote-server
journalctl -u decoremote-server -f
```

## Con Docker

```bash
docker compose -f deploy/docker-compose.yml up -d --build
```

## Dai client

```
decoremote --server IP_DEL_VPS:21116
```

Poi *Impostazioni → Verifica rete* per controllare che UDP arrivi.

## Compilare il binario Linux statico

```bash
rustup target add x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=gcc cargo build --release -p decoremote-server --target x86_64-unknown-linux-musl
```

Il server non ha rate limiting né autenticazione propria: chiunque raggiunga la porta può registrare un ID.
La sicurezza delle sessioni sta nella password dell'host (Noise + Argon2id).
