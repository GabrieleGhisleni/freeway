# Deploy su Raspberry Pi con Cloudflare Tunnel

Il binario Rust si compila **sul PC** (Docker) e si carica sul Pi: sul Pi non serve Rust né Docker.
Il server ascolta solo su `127.0.0.1`; a esporlo su internet ci pensa un Cloudflare Tunnel, cioè una connessione **in uscita** dal Pi (nessuna porta da aprire sul router).
Il deploy è uno script lanciato a mano dal PC: niente CI, niente hook git.

```
client ──HTTPS──> Cloudflare ──tunnel──> cloudflared (Pi) ──> freeway 127.0.0.1:4100
```

Servono: un Pi con Linux a 64 bit (`uname -m` → `aarch64`) raggiungibile via SSH (qui l'host `rpi` in `~/.ssh/config`, utente con `sudo` senza password), un dominio su Cloudflare, Docker sul PC.

## Secrets

Le chiavi dei provider e `ROUTER_TOKENS` stanno **solo** nel `~/freeway/.env` del Pi (permessi 600). Non passano da git né da GitHub, e `deploy.sh` non le tocca.

```bash
deploy/env.sh keys         # nomi delle variabili sul Pi, valori nascosti
deploy/env.sh set GEMINI_API_KEY   # chiede il valore senza mostrarlo, aggiorna e riavvia
deploy/env.sh new-token    # aggiunge un token casuale a ROUTER_TOKENS, lo stampa una volta, riavvia
deploy/env.sh edit         # apre il .env remoto nell'editor
```

Un token per app: per togliere l'accesso a un'app lo cancelli con `edit` e il servizio si riavvia.

## Prima installazione

**1. Sul Pi: cartella, `.env`, servizio.**

```bash
mkdir -p ~/freeway/data
nano ~/freeway/.env        # modello: .env.example (chiavi dei provider e ROUTER_TOKENS)
chmod 600 ~/freeway/.env
```

Poi copia [deploy/freeway-rust.service](../deploy/freeway-rust.service) in `/etc/systemd/system/freeway.service` correggendo `User` e i percorsi (`/home/pi/freeway` → la tua home), e abilitalo:

```bash
sudo systemctl daemon-reload && sudo systemctl enable freeway
```

**2. Dal PC: primo deploy.**

```bash
deploy/deploy.sh
```

Compila, carica binario e `config.yaml` e avvia il servizio. Prova in locale sul Pi:

```bash
curl -H "Authorization: Bearer <token>" http://127.0.0.1:4100/health
```

**3. Sul Pi: Cloudflare Tunnel.**
Installa `cloudflared` (pacchetto `linux-arm64` da github.com/cloudflare/cloudflared/releases), poi:

```bash
cloudflared tunnel login          # apri il link nel browser, scegli il dominio e autorizza
cloudflared tunnel create freeway
cloudflared tunnel route dns freeway freeway.tuodominio.com
```

Crea `/etc/cloudflared/config.yml` e copia lì il file di credenziali del tunnel (`~/.cloudflared/<ID>.json`, `chmod 600`):

```yaml
tunnel: <ID del tunnel>
credentials-file: /etc/cloudflared/<ID del tunnel>.json
ingress:
  - hostname: freeway.tuodominio.com
    path: ^/admin
    service: http_status:404          # /admin/* resta raggiungibile solo dal Pi
  - hostname: freeway.tuodominio.com
    service: http://127.0.0.1:4100
  - service: http_status:404
```

```bash
sudo cloudflared service install
```

**4. Prova da fuori.**

```bash
curl -i https://freeway.tuodominio.com/health                                  # 401
curl -i -H "Authorization: Bearer $TOKEN" https://freeway.tuodominio.com/health   # 200
```

Il dominio in uso è `freeway.gabrieleghisleni.dev` e il tunnel si chiama `freeway`. Come chiamare il servizio: [uso.md](uso.md).

## Aggiornare

```bash
deploy/deploy.sh
```

Esegue `cargo test`, compila per aarch64, carica binario e `config.yaml`, riavvia e controlla che risponda. Prima di partire rifiuta se non sei su `main` o se ci sono modifiche non committate, così sul Pi finisce solo codice che è nel repo.

| Opzione | Effetto |
|---|---|
| `--no-build` | riusa l'ultima build |
| `--skip-tests` | salta `cargo test` |
| `--force` | permette un altro branch o modifiche non committate |

Variabili: `DEPLOY_HOST` (default `rpi`), `DEPLOY_DIR` (default `freeway`, relativa alla home del Pi).
Sul Pi, `~/freeway/VERSION` dice quale commit è in esecuzione.

`config.yaml` viene **sovrascritto** a ogni deploy con quello del repo: le modifiche alla configurazione si fanno nel repo, non sul Pi.
Un deploy non cancella le pause dei provider né altro stato in memoria: il riavvio le dimentica (vedi il [README](../README.md#5-quando-un-provider-non-risponde)).

## Operazioni

```bash
ssh rpi 'journalctl -u freeway -f'                 # log del router
ssh rpi 'journalctl -u cloudflared -f'             # log del tunnel
ssh rpi 'cat freeway/VERSION; systemctl is-active freeway cloudflared'
ssh rpi 'curl -s -H "Authorization: Bearer $(grep ^ROUTER_TOKENS freeway/.env | cut -d= -f2 | cut -d, -f1)" http://127.0.0.1:4100/admin/status'
```

**Sicurezza.** Il token è l'unica difesa: chi lo ha consuma le quote gratuite dei provider. Usa token lunghi e casuali (uno per app) e, se uno trapela, toglilo con `deploy/env.sh edit`. Su internet i prompt passano da Cloudflare e, con i modelli gratuiti, dal provider che li serve: niente dati sensibili.

**Annullare tutto.** `sudo systemctl disable --now cloudflared` chiude subito l'accesso da internet; per cancellare il tunnel: `cloudflared tunnel delete freeway` e il record DNS dalla dashboard Cloudflare.
