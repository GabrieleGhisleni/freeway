# freeway: istruzioni per gli agenti

Server **OpenAI-compatibile** in Rust che smista le richieste tra provider LLM gratuiti per tier di intelligenza.
Un solo binario (`rust/`); la vecchia versione Python è stata rimossa. Documentazione e commenti dei documenti in **italiano**, codice e commit in inglese.

Per capire il progetto: [README.md](README.md) (scelta dei modelli, `config.yaml`), [docs/uso.md](docs/uso.md) (come si chiama), [docs/deploy.md](docs/deploy.md) (Pi e tunnel).

## Comandi

```bash
cd rust && cargo test                 # unit + integrazione con provider finti (rust/tests/routing.rs), senza rete
cd rust && cargo build --release && cd .. && ./rust/target/release/freeway    # locale, http://127.0.0.1:4100
deploy/deploy.sh                      # test + build aarch64 + upload + restart sul Pi (host ssh `rpi`)
deploy/env.sh keys|set|new-token|edit # .env del Pi (chiavi e ROUTER_TOKENS)
```

- `deploy.sh` accetta solo `main` pulito (altrimenti `--force`). Committa **prima** di fare deploy.
- Dopo il restart il server impiega circa **un minuto** a mettersi in ascolto (sonda i modelli NVIDIA): lo script aspetta fino a 90 s. Se dice "did not come up", guarda `ssh rpi 'journalctl -u freeway -n 30'` prima di allarmarti.
- Il Pi tiene stato in `~/freeway/data/` (`scores.json`, `health.json`). `deploy.sh` non lo tocca; il servizio può scrivere solo lì.

## Mappa del codice (`rust/src/`)

`config.rs` (config.yaml) · `catalog.rs` (scoperta e fusione dei modelli) · `normalize.rs` · `scores.rs` (Artificial Analysis) · `router.rs` (scelta, fallback, chiamate) · `health.rs` (pause, contatori, persistenza) · `app.rs` (HTTP) · `systemone.rs` (Decision API) · `fake.rs` · `http.rs` · `util.rs`.

## Regole di progetto

- **Modelli senza punteggio** (`unrated`): nessun tier li sceglie finché c'è un endpoint valutato disponibile; sono l'ultima risorsa prima dell'errore (non con `x-router-fallback: none`).
- **Errori 503, mai 502/504**: Cloudflare sostituisce quelli con una pagina HTML e il chiamante non vedrebbe il JSON.
- **Pause**: un 429 mette in pausa l'endpoint per il `Retry-After` del provider (tetto 6 h); due modelli limitati di fila mettono in pausa l'intero provider. Stato in `data/health.json`, scritto subito sui fallimenti e al massimo ogni 30 s sui successi.
- Ogni nuovo endpoint o comportamento visibile ha un test in `rust/tests/routing.rs` (o unit test nel modulo).
- I test passano `health_file: null` nello YAML per non scrivere su disco: fallo anche nei nuovi.

## Postman (da tenere aggiornato)

La collection **freeway** vive nel workspace personale "GG" e questo repo ne ha la copia locale in `postman/` (config in `.postman/resources.yaml`).
**Ogni volta che aggiungi, rimuovi o cambi un endpoint, un header o un body di esempio, aggiorna anche Postman**, insieme a [docs/uso.md](docs/uso.md):

1. Modifica le richieste in `postman/collections/freeway/*.request.yaml` (un file per richiesta, `order` decide la posizione; auth bearer `{{token}}`, URL `{{baseUrl}}/...`). Gli endpoint `/admin/*` usano `{{adminUrl}}`, perché dal tunnel pubblico rispondono 404.
2. `postman workspace lint` deve dire "No issues found".
3. `postman workspace push --yes -m "cosa è cambiato"` (default: crea/aggiorna, **non** cancella nulla nel cloud; non usare `--push-strategy force-sync`).
4. Se qualcuno ha modificato la collection dall'app, prima `postman workspace pull --yes` e rivedi il diff.

**Segreti:** `postman/environments/` è in `.gitignore` perché `freeway.environment.yaml` contiene il token vero. Non committarlo, non incollarlo in chat, non rimuovere la riga dal `.gitignore`. Se serve ricrearlo: `postman workspace pull --yes` lo riscarica (poi controlla che resti ignorato e rimuovi le altre collection/environment del workspace che non appartengono a freeway).

## Segreti in generale

Le chiavi dei provider e `ROUTER_TOKENS` stanno solo nel `.env` (locale) e in `~/freeway/.env` sul Pi. Mai in git, in `config.yaml`, nei log o nei messaggi. Usa `deploy/env.sh` per cambiare quelli del Pi.
