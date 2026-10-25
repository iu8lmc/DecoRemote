# DecoRemote

Desktop remoto scritto in Rust, ispirato a [RustDesk](https://github.com/rustdesk/rustdesk).
Ci si collega a un altro computer con **ID + password**, senza aprire porte sul router del computer controllato.

## Struttura

| Crate | Eseguibile | Ruolo |
|---|---|---|
| `crates/deco-proto` | – | Messaggi, framing e canale cifrato end-to-end (Noise) |
| `crates/deco-net` | – | Collegamento diretto QUIC/UDP con hole punching |
| `crates/deco-codec` | – | Codec video AV1 a fasce (libaom real-time) |
| `crates/deco-server` | `decoremote-server` | Rendezvous + relay: registra gli ID e collega viewer e host |
| `crates/deco-app` | `decoremote` | Client con interfaccia egui: fa sia da host che da viewer |

## Come funziona

```
 Viewer ──TCP──►  decoremote-server  ◄──TCP── Host (ID registrato)
    │                   │ relay dei byte                │
    └──────── Noise NNpsk0 end-to-end (il server vede solo cifrato) ───────┘
```

1. L'host apre una connessione di controllo verso il server e registra il proprio ID (9 cifre, generato al primo avvio).
2. Il viewer chiede `Connect{ID}`; il server avvisa l'host, che apre una seconda connessione; il server le unisce.
3. Sul canale unito parte l'handshake **Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s**. Il PSK è derivato dalla password con **Argon2id** (salt = ID host): con password errata l'handshake fallisce.
4. L'host cattura lo schermo (DXGI via `scrap`, vendorizzato in `vendor/scrap` con una piccola patch per l'origine dei monitor), lo codifica in JPEG e lo invia; il viewer invia mouse e tastiera, che l'host inietta con `enigo`. Gli appunti (testo) sono sincronizzati nei due sensi.

## Uso

```powershell
cargo build --release

# 1. Server (su una macchina raggiungibile da entrambi; porta TCP 21116)
.\target\release\decoremote-server.exe 0.0.0.0:21116

# 2. Client, su entrambi i computer
.\target\release\decoremote.exe --server IP_DEL_SERVER:21116
```

Se non si indica `--server`, il client usa `15.235.240.243:21116` (vedi `DEFAULT_SERVER` in `crates/deco-app/src/config.rs`); l'indirizzo si cambia anche da *Impostazioni*.

Il client mostra il proprio ID e la password; sull'altro computer si inseriscono ID e password del partner e si preme **Connetti**.
Opzioni: `--connect ID --password PW` per collegarsi subito all'avvio. `DECOREMOTE_HOME=<cartella>` sposta il file di configurazione (utile per provare due istanze sullo stesso PC).

Configurazione in `%APPDATA%\DecoRemote\config.json`.

## Test

```powershell
cargo test    # cifratura + relay end-to-end con server reale
```

## Limiti attuali / prossimi passi

- Si trasmette un monitor alla volta (selezionabile dalla barra del viewer quando l'host ne ha più di uno).
- Latenza end-to-end su due reti diverse non ancora misurata (i test sono su una sola macchina). Possibile passo: codifica hardware AV1 su GPU RTX 40.
- Il collegamento diretto funziona se i NAT lo permettono (non con NAT simmetrici/CGNAT): altrimenti si ripiega sul relay TCP. Il server non ha rate limiting.
- Non controlla finestre con privilegi elevati / schermata UAC e login se l'app gira come utente normale (servirebbe un servizio Windows).
- La conferma lato host è attiva di default (30 s, poi rifiuto automatico). Si può disattivare da Impostazioni per l'accesso non presidiato, ma allora l'accesso è protetto solo dalla password.


## Codec video

Il video usa **AV1** (libaom in modalità real-time, `tune_content=screen` con palette e IntraBC), 4:4:4 per il testo nitido. Lo schermo è diviso in 8 fasce orizzontali da ~180 righe, ognuna con il proprio flusso: si codificano e decodificano in parallelo e **le fasce non cambiate non vengono né codificate né inviate**. Il JPEG resta come ripiego; il codec si negozia a ogni sessione.

Misure su questo PC (Ryzen 9 5950X, schermo 3440x1440, `cargo run --release -p deco-codec --example bench`):

| Scenario | Codifica | Decodifica | Banda a 60 fps |
|---|---|---|---|
| Schermo fermo | ~1 ms | ~0 ms | ~0 |
| Piccole modifiche (digitazione) | ~12 ms | ~7 ms | ~4 Mbit/s |
| Scorrimento a tutto schermo | ~27 ms | ~13 ms | ~10 Mbit/s |

Per compilare servono **CMake** e **NASM 2.16.x** (libaom 3.11 non riconosce NASM 3.x): `winget install Kitware.CMake` e `winget install NASM.NASM --version 2.16.03`.

## Cursore

La cattura DXGI non include il puntatore: l'host ne invia forma (ricavata con GDI, anche per cursori monocromatici/a inversione) e posizione come messaggi separati, e il viewer lo disegna da sé. Mentre muovi il mouse il cursore sta dove sta il mouse locale, senza attendere la rete; quando lo lasci fermo torna alla posizione riportata dall'host.

## Trasporto: collegamento diretto QUIC con ripiego sul relay

Dopo che il server ha messo in contatto viewer e host, i due si scambiano i propri indirizzi UDP (scoperti dal server, che ascolta anche in UDP sulla stessa porta) e provano un **collegamento diretto QUIC** con hole punching, senza passare dal server. Se non riesce entro ~2,5 s (NAT simmetrico, UDP bloccato), la sessione prosegue sul relay TCP come prima: la scelta è automatica.

- Il canale Noise end-to-end resta identico sopra QUIC; il TLS di QUIC (certificato autofirmato) serve solo perché il protocollo lo richiede.
- Controllo di congestione **BBR**: code corte, meno latenza di CUBIC/TCP.
- **Stream separate**: comandi, cursore e appunti viaggiano su una stream, il video su un'altra, quindi un pacchetto perso nel video non li blocca. Test (`cargo test -p deco-net --release -- --nocapture`) con 3% di perdite, 30 ms di RTT e video a piena velocità: controllo con p50 ~62 ms e p95 ~121 ms.
- Il viewer mostra nella barra `QUIC diretto N ms` oppure `relay TCP`.
- Il server deve avere aperta anche la **porta UDP** (la stessa del TCP, 21116). Il firewall di Windows può chiedere il consenso al primo avvio del client.
- `DECOREMOTE_NO_UDP=1` disattiva il collegamento diretto (utile per diagnosi).

## Accesso non vigilato

In *Impostazioni → Accesso non vigilato* si può far sì che il computer resti raggiungibile senza nessuno davanti:

- **Senza conferma:** chi conosce ID e password entra senza che l'host debba consentire.
- **Resta attivo:** chiudendo la finestra il programma si riduce a icona e l'host resta online; per uscire davvero c'è *Esci da DecoRemote*.
- **Avvio con Windows:** il programma parte da solo (ridotto a icona, opzione `--minimized`) quando l'utente accede. Usa la chiave `Run` dell'utente, senza privilegi di amministratore.
- **Password fissa:** si può impostare una password propria (almeno 8 caratteri) al posto di quella generata.
- **Protezione:** dopo 5 password errate l'host rifiuta ogni richiesta per 10 minuti.

Limiti: serve una sessione Windows aperta (la schermata di blocco, il login e le finestre UAC non sono visibili: non c'è un servizio di sistema). ID e password danno il controllo completo del computer: usare una password lunga.

## Chat e trasferimento file

Durante una sessione, nella barra del viewer c'è **Chat e file** (a destra si apre un pannello); sull'host la stessa chat e l'elenco dei file compaiono nella finestra principale. Tutto viaggia nella stessa sessione cifrata, senza passare da altri servizi.

- **Chat in tempo reale** in entrambe le direzioni, per dare piccole notizie all'utente remoto. Se la finestra dell'host è ridotta a icona, un messaggio la riporta in vista (senza rubare il focus).
- **Invia file, immagini e log ADIF** (`.adi`, `.adif`) all'host: pulsanti con filtro dedicato oppure trascinando i file sulla finestra del viewer. Arrivano in `DocumentiDecoRemoteRicevuti` sull'host, senza mai sovrascrivere (`nome (1).adi`).
- **Scarica dall'host:** *Sfoglia* le cartelle del PC remoto (documenti, desktop, dischi) oppure **Cerca log ADIF**, che trova i file `.adi/.adif` nelle cartelle usuali (WSJT-X, JTDX, Decodium, Documenti…) dal più recente. I file scaricati finiscono in `DocumentiDecoRemoteScaricati` sul viewer.
- I file viaggiano a pezzi con controllo di flusso (finestra di 256 KB), così non soffocano video e comandi; limite di 2 GB a file. I nomi vengono ripuliti (niente percorsi, caratteri vietati o nomi riservati di Windows) e il file compare solo a trasferimento completo.

Il protocollo è cambiato: host e viewer devono avere la stessa versione.

## Licenza

MIT, vedi [LICENSE](LICENSE). `vendor/scrap` è una copia modificata di [scrap](https://github.com/quadrupleslap/scrap), anch'essa con licenza MIT.
