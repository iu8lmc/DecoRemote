# Prova di DecoRemote fra due reti diverse

Cartella pronta da copiare: `decoremote.exe` (client) e `decoremote-server.exe` (server).
Sono eseguibili autonomi: non serve installare nulla sull'altro PC.

## Cosa serve

1. **Un server raggiungibile da entrambe le reti**, cioè su una macchina con IP pubblico
   (un VPS, oppure un PC su una rete con port forwarding). Questo PC **non va bene**: la sua
   connessione è dietro CGNAT (il secondo salto è `100.64.0.1`), quindi da fuori non è raggiungibile.
2. **Due computer su reti diverse**, ad esempio questo PC sul Wi-Fi di casa e l'altro su un hotspot del telefono
   o in un altro luogo.

## Passi

**1. Server** (sulla macchina con IP pubblico). Aprire nel firewall: **TCP 21116**, **UDP 21116** e **UDP 21117**
(la seconda porta UDP serve alla verifica del NAT).

```powershell
.\decoremote-server.exe 0.0.0.0:21116
```

Su Linux: `cargo build --release -p decoremote-server` e poi `./decoremote-server 0.0.0.0:21116`
(il server non ha dipendenze native).

**2. Client su entrambi i PC**, da terminale (si vedono così anche i log):

```powershell
.\decoremote.exe --server IP_DEL_SERVER:21116
```

**3. Verifica rete.** In ogni client: *Impostazioni → Verifica rete*. Esiti possibili:

| Messaggio | Significato |
|---|---|
| `OK: indirizzo pubblico …` | da questa rete il collegamento diretto è possibile |
| `NAT simmetrico …` | il collegamento diretto non è possibile: si userà il relay |
| `UDP non raggiunge il server …` | UDP bloccato o porte del server chiuse |

**4. Connessione.** Sul PC "host" leggere ID e password; sull'altro inserirli e premere *Connetti*
(sull'host consentire la richiesta).

**5. Cosa guardare.** Nella barra del viewer compare `QUIC diretto N ms` (collegamento diretto, N = RTT) oppure
`relay TCP`. Nei log del terminale: `percorso dati: QUIC diretto` / `relay TCP`.

## Cosa mi interessa sapere

- L'esito di *Verifica rete* su ciascuna delle due reti.
- Il percorso usato (`QUIC diretto` o `relay TCP`) e l'RTT mostrato.
- Fluidità percepita: scrivere, muovere il mouse, scorrere una pagina; fps e Mbit/s nella barra.
- Per confronto, ripetere con `DECOREMOTE_NO_UDP=1` (forza il relay TCP):

```powershell
$env:DECOREMOTE_NO_UDP = "1"; .\decoremote.exe --server IP_DEL_SERVER:21116
```
