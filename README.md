# freeway

Server locale **OpenAI-compatibile** che smista le richieste tra i provider LLM gratuiti
(OpenRouter, OpenCode Zen, Google AI Studio) e, se uno non risponde,
passa al successivo. Il chiamante sceglie un **livello di intelligenza** (`smart`, `balanced`, `fast`)
oppure un **modello preciso**; il router trova un provider che lo serva davvero.

## Avvio rapido

```bash
cd freeway
cp .env.example .env      # poi compila .env (vedi sotto)
uv sync
uv run llm-router         # http://127.0.0.1:4100
```

Opzioni: `uv run llm-router --port 4200 --host 127.0.0.1 --config altro.yaml`.

### Versione Rust

Esiste una versione equivalente in Rust (`rust/`): stessi endpoint, stesso `config.yaml`, stesso `.env`, stessa cache `data/scores.json`.
È un singolo binario di ~5 MB, senza Python e `uv`: pensata per il Raspberry Pi.

```bash
cd rust && cargo build --release           # produce rust/target/release/llm-router
cd .. && ./rust/target/release/llm-router  # avvialo dalla cartella del progetto (legge ./config.yaml e ./.env)
```

Stesse opzioni: `--config`, `--host`, `--port`. Test: `cd rust && cargo test`.
Il `config.yaml` viene cercato nella cartella corrente (o in `$ROUTER_CONFIG`), quindi avvia dalla cartella del progetto.
Dopo ogni modifica a `config.yaml` o `.env` **riavvia** il server.

### Variabili in `.env`

| Variabile | A cosa serve | Dove si ottiene |
|---|---|---|
| `ROUTER_TOKENS` | password per chi chiama il router; **una o più, separate da virgola** (`token1,token2`) | `python3 -c "import secrets; print(secrets.token_urlsafe(32))"` per ognuna |
| `ARTIFICIAL_ANALYSIS_API_KEY` | punteggi di intelligenza dei modelli | artificialanalysis.ai (gratis) |
| `OPENROUTER_API_KEY` | provider OpenRouter | openrouter.ai |
| `OPENCODE_API_KEY` | provider OpenCode Zen | opencode.ai |
| `GEMINI_API_KEY` | provider Google AI Studio | aistudio.google.com |
| `LOG_LEVEL` | `INFO` (default) o `DEBUG` | — |

Un provider senza chiave viene **saltato** (lo dice `/admin/status`). Senza `ROUTER_TOKENS` il server è aperto a
chiunque lo raggiunga e compare un warning. Ogni token è valido da solo: per dare accesso a una persona o a un'app
basta aggiungerne uno alla lista, per toglierlo lo cancelli e riavvii (il vecchio nome `ROUTER_TOKEN` funziona ancora). `.env` è nel `.gitignore`: non metterci mai le chiavi altrove.

## Come si chiama

È un normale endpoint OpenAI: `base_url = http://127.0.0.1:4100/v1`, `api_key = <uno dei ROUTER_TOKENS>`
(il token si manda come `Authorization: Bearer ...`; vale anche l'header `x-api-key`).

```bash
curl http://127.0.0.1:4100/v1/chat/completions \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"model":"smart","messages":[{"role":"user","content":"Ciao!"}]}'
```

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:4100/v1", api_key="<uno dei ROUTER_TOKENS>")
r = client.chat.completions.create(model="smart", messages=[{"role": "user", "content": "Ciao!"}])
print(r.choices[0].message.content)
```

Funzionano streaming (`stream: true`), tool calling e immagini, **se** il modello scelto li supporta
(il router salta da solo gli endpoint che dichiarano di non supportarli). Chat: `/v1/chat/completions` (più la Decision API, solo Rust). Niente embeddings, niente formato Anthropic.

### Cosa mettere in `model`

| Valore | Cosa succede |
|---|---|
| `smart`, `balanced`, `fast` | **Tier**: prova i modelli migliori del tier, poi quelli dei tier di riserva (`fallback` in `config.yaml`) |
| `gemini-3.8-flash` | **Sempre lo stesso modello**, su qualunque provider lo abbia. Non scala ad altri modelli |
| `openrouter/nvidia/nemotron-3.5-super-vl:free` | **Un solo endpoint** di un solo provider (`<provider>/<id del provider>`) |

I nomi validi si leggono con `GET /v1/models`.

### Ragionamento (reasoning)

Il router non gestisce il ragionamento: **inoltra il body così com'è**, quindi si controlla con il parametro standard OpenAI `reasoning_effort`,
da mandare in ogni richiesta. Se non lo mandi, ogni modello usa il suo comportamento di default (i modelli «ragionanti» pensano a lungo).

```python
client.chat.completions.create(model="fast", reasoning_effort="none", messages=[...])
```

| Valore | Verificato |
|---|---|
| `"none"` | **Sì**, su OpenRouter e Google: risposta in meno di un secondo e senza token di ragionamento (un modello `fast` con ragionamento: ~9 s e ~390 token per `17*23`). Su OpenCode non è stato possibile provarlo (429) |
| `"minimal"`, `"low"`, `"medium"`, `"high"` | No. Su OpenRouter `low` riduce il ragionamento ma non lo toglie; i valori più alti non sono stati provati |
| non mandarlo | Default del modello: è la scelta per i tier alti (`smart`, `balanced`), dove il ragionamento serve |

- Usa `reasoning_effort`, non l'oggetto `reasoning: {...}` di OpenRouter: Google lo rifiuta con 400 (campo sconosciuto) e il router passa al provider successivo.
- Un modello che non accetta il valore risponde 400: il router non lo mette in pausa e prova l'endpoint dopo. Se lo rifiutano tutti, l'errore torna a te.
- Non c'è ancora un default per tier in `config.yaml`: `fast` senza ragionamento oggi richiede `reasoning_effort: "none"` in ogni richiesta.

### Header opzionali (della richiesta)

| Header | Valori | Effetto |
|---|---|---|
| `x-router-fallback` | `auto` (default) | i tier scalano ai tier di riserva; un modello per nome non cambia modello |
| | `none` | resta nel tier/modello richiesto; se non c'è niente risponde 503 |
| | `tier` | anche un modello per nome, se fallisce, scala ai modelli del suo tier |
| `x-router-session` | un id qualsiasi | tiene la conversazione sullo stesso modello finché risponde (120 min) |

### Header della risposta: chi ha risposto

`x-router-model`, `x-router-tier` (o `unrated`), `x-router-provider`, `x-router-upstream-model`,
`x-router-attempts` (tentativi fatti, compreso quello riuscito). Guardali: un tier può **declassare**
in silenzio, per esempio `smart` può rispondere con un modello `balanced`. Per non declassare usa
`x-router-fallback: none`.

## Come decide

### 1. Quali modelli esistono
Ogni 30 minuti il router rilegge l'elenco modelli di ogni provider attivo e tiene solo quelli **gratuiti**.
OpenRouter e OpenCode indicano il prezzo (0); Google no, quindi lì si considera
gratuito tutto ciò che è elencato e si filtra con `exclude:` ciò che non serve (embeddings, immagini, modelli a pagamento).

### 2. Lo stesso modello su provider diversi
I nomi vengono normalizzati (tolti `:free`, `-free`, date, prefisso del produttore): `nvidia/nemotron-3.5-lightning:free`
e `nemotron-3.5-lightning-free` diventano un solo modello con due endpoint, ordinati per `priority` (più basso = prima).
Quello che non si aggancia da solo si dichiara in `aliases:`. Attenzione: stesso modello non significa identico
(quantizzazione, contesto e parametri possono cambiare da provider a provider).

### 3. I tier
Il punteggio è l'**Intelligence Index di Artificial Analysis** (0–65 circa), scaricato ogni 24 h e salvato in `data/scores.json`.
Se un modello ha più varianti (reasoning high/low) vale la più bassa. Le soglie sono in `config.yaml`:

| Tier | Soglia | Riserva |
|---|---|---|
| `smart` | ≥ 20 | `balanced`, poi `fast` |
| `balanced` | ≥ 10 | `fast` |
| `fast` | < 10 | — |

I tier sono **relativi ai modelli gratuiti**: il migliore gratuito vale circa 29, i migliori a pagamento arrivano a 65.
Dentro il tier si prova prima il punteggio più alto.

### 4. Modelli senza punteggio
Il piano gratuito di Artificial Analysis non pubblica l'indice di molti modelli. Un modello senza punteggio è
**non valutato**: nessun tier lo sceglie, ma si può chiamare per nome. `GET /admin/status` elenca gli `unrated`.
Per metterli nei tier dai un voto in `models:` (sulla stessa scala). I voti scritti a mano in `config.yaml`
marcati come *ESTIMATES* sono stime, non misure.

### 5. Quando un provider non risponde
| Errore | Cosa fa il router |
|---|---|
| 429 (limite) | pausa dell'endpoint: usa `Retry-After`, altrimenti 60 s raddoppiando fino a 15 min. Dopo due 429 di modelli diversi dello stesso provider, pausa **dell'intero provider** |
| 401 (chiave) | pausa di tutto il provider per 10 min |
| 402 / 403 / 404 | pausa di quel modello per 30 min (es. «non disponibile nel tuo paese») |
| 5xx, timeout, risposte vuote | backoff da 20 s fino a 10 min |
| 400 (richiesta sbagliata) | nessuna pausa: se tutti rifiutano, l'errore originale torna al chiamante |

Anche errori nascosti in una risposta 200 vengono riconosciuti. In streaming il router aspetta il primo chunk valido
prima di impegnarsi; dopo il primo byte non può più cambiare provider. Si provano al massimo 8 endpoint per richiesta.
Le pause sono **in memoria**: se riavvii il server le dimentica.

### Errori che può restituire
| Codice | Significato |
|---|---|
| 401 | token mancante o sbagliato |
| 404 | modello o tier sconosciuto (il messaggio elenca i tier) |
| 502 | tutti gli endpoint provati hanno fallito; `error.tried` dice cosa è successo a ciascuno |
| 503 `all_providers_busy` | tutto è in pausa; header `retry-after` con i secondi |
| 503 `no_endpoint` | nessun provider caricato, tier vuoto con `fallback: none`, o nessun modello supporta tool/immagini |

## Decision API (solo versione Rust)

`POST /v1/systemone` (alias `POST /api/alpha/decisions`, il nome usato da OpenRouter) risponde a **domande tipizzate**
su uno `state`, nello stesso formato dei modelli «System One» come Jev di TypeSafe: invece di testo restituisce probabilità.
`model` è un tier o un modello del router (`fast`, `balanced`, `smart`, un nome preciso).

```bash
curl http://127.0.0.1:4100/v1/systemone -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d '{
  "model": "balanced",
  "state": "Mi avete addebitato due volte l'abbonamento. Rimborsatemi.",
  "questions": {
    "billing": {"type": "noul", "instructions": "Parla di fatturazione?"},
    "team":    {"type": "choice", "instructions": "Quale team?", "criteria": {"billing": "addebiti, rimborsi", "tech": "bug e disservizi"}},
    "anger":   {"type": "score", "instructions": "Quanto è arrabbiato?", "criteria": ["calmo", "scocciato", "furioso"]}
  }
}'
```

| Tipo | `criteria` | Risposta |
|---|---|---|
| `noul` | opzionale: `{"true": ..., "false": ...}` | `noul`: probabilità del sì (0–1) |
| `choice` | obbligatorio: `{opzione: descrizione}`, 2–255 opzioni | `choice`, `probabilities` per opzione, `confidence` |
| `score` | obbligatorio: array di 2–10 livelli, dal più basso al più alto | `score` (media pesata dei livelli), `probabilities`, `legend`, `confidence` |

`state` può essere una stringa, un oggetto o un array; le immagini si mettono nell'array come `{"type":"image_url","image_url":{"url":...}}`
(il router sceglie un modello che le supporta). Non sono supportati `box` e `assets`. La risposta ha `model`, `answers` (stesse chiavi delle domande)
e `usage`, più gli header `x-router-*`; valgono anche `x-router-fallback` e `x-router-session`.

**Attenzione:** qui le risposte vengono dai modelli di chat gratuiti, ai quali si chiede un JSON. Le probabilità sono le stime del modello,
**non calibrate** come quelle di Jev, e `confidence` la calcola il router dalla dispersione delle probabilità (1 = tutta su un'opzione, 0 = uniforme).
Se un modello risponde con qualcosa di inutilizzabile viene messo in pausa e si prova un altro (fino a 3 modelli). I modelli «ragionanti» del tier `fast`
possono impiegare decine di secondi: per decisioni rapide conviene un modello non ragionante, per nome.

## Endpoint di servizio
Tutti richiedono il token, compreso `/health`.

| Endpoint | Cosa fa |
|---|---|
| `GET /v1/models` | tier e modelli con tier, punteggio e provider |
| `GET /v1/models/{id}` | un solo tier o modello (anche `<provider>/<id>`); 404 `model_not_found` se non esiste. Solo versione Rust |
| `GET /admin/status` | provider attivi/saltati, stato dei punteggi, modelli per tier con pause ed errori, elenco `unrated` |
| `POST /admin/refresh` | rilegge subito gli elenchi dei provider |
| `GET /health` | `{"status":"ok"}` |

## Configurazione (`config.yaml`)

- `tiers`: nomi, soglie e riserve dei livelli.
- `providers`: per ognuno `base_url`, `api_key_env`, `priority`, `include`/`exclude` (glob sugli id del provider), `enabled`.
  Per aggiungerne uno compatibile OpenAI (per esempio Groq, Cerebras o NVIDIA) basta un blocco con `kind: openai`, `base_url` e `api_key_env`.
- `models`: correzioni tue. `score` (ricalcola il tier), `tier` (forza), `disabled: true` (esclude).
  La chiave può essere qualunque nome con cui il modello compare in `/v1/models`.
- `aliases`: nomi da trattare come lo stesso modello.
- `exclude`: glob sui nomi dei modelli da escludere ovunque.
- Tempi: `first_token_timeout`, `stream_idle_timeout`, `request_timeout`, `max_attempts`, `refresh_minutes`.

Un tier puoi anche crearlo a mano: con `min_score: 101` nessun modello ci arriva da solo, e lo riempi con
`models: { nome: { tier: mio-tier } }`.

## Stato attuale e limiti

- **OpenCode** risponde 429 «Free usage limit reached» a tutti i suoi modelli gratuiti, anche con chiave valida;
  il router lo mette in pausa e riprova. Finché dura, `smart` risponde da Google.
- **`inkling-2-small`** è disabilitato: 403 «non disponibile nel tuo paese» su ogni provider.
- **OpenRouter** nasconde alcuni modelli gratuiti (i Nemotron di NVIDIA, Liquid) se nelle impostazioni di privacy del
  tuo account non permetti i modelli che possono registrare i prompt. Sui modelli gratuiti i prompt possono essere
  registrati dal provider: non mandare dati sensibili.
- **Google**: i Pro, `omni`, `gemini-4-*` non hanno quota gratuita; i `gemini-2.5-*` sono chiusi ai nuovi utenti.
- I limiti gratuiti cambiano spesso: modelli e quote compaiono e spariscono, il catalogo si riallinea da solo.
- Il router non conosce le quote reali: le scopre dai 429.

## Deploy su Raspberry Pi

Serve un Raspberry Pi con Linux a 64 bit (Pi 3/4/5 con Raspberry Pi OS 64-bit: `uname -m` deve dire `aarch64`). La versione Rust è un solo binario, senza Python.

**1. Compila sul Pi** (la strada più semplice; la prima build è lenta per `lto`, ma si fa una volta):

```bash
# sul Pi
sudo apt install -y build-essential git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # poi riapri la shell
git clone https://github.com/GabrieleGhisleni/freeway.git ~/freeway && cd ~/freeway/rust
cargo build --release          # produce rust/target/release/llm-router
```

In alternativa si può compilare sul PC per `aarch64-unknown-linux-gnu` e copiare solo il binario: **non è stato provato** in questo progetto (serve il linker `gcc-aarch64-linux-gnu`, perché `ring` compila codice C).

**2. Configurazione** (il binario cerca `config.yaml` e `.env` nella cartella da cui parte):

```bash
cd ~/freeway
cp .env.example .env && chmod 600 .env     # compila le chiavi e ROUTER_TOKENS
mkdir -p data                              # cache dei punteggi, deve essere scrivibile
./rust/target/release/llm-router --host 127.0.0.1 --port 4100   # prova a mano
curl -H "Authorization: Bearer <token>" http://127.0.0.1:4100/health
```

**3. Servizio systemd** (parte all'avvio e si riavvia da solo):

```bash
sudo cp deploy/llm-router-rust.service /etc/systemd/system/llm-router.service
sudo nano /etc/systemd/system/llm-router.service   # controlla User e i percorsi /home/pi/freeway
sudo systemctl daemon-reload && sudo systemctl enable --now llm-router
journalctl -u llm-router -f                        # log
```

**4. Raggiungerlo dagli altri dispositivi.** Il file di esempio ascolta su `0.0.0.0`, cioè su tutta la rete locale. Imposta sempre `ROUTER_TOKENS` e non esporlo
su internet: usa la rete di casa, una VPN (Tailscale) oppure `--host 127.0.0.1` dietro un reverse proxy con HTTPS. Poi nei client: `base_url = http://<ip-del-pi>:4100/v1`.

Per aggiornare: `git pull && cd rust && cargo build --release && sudo systemctl restart llm-router`. Le pause dei provider sono in memoria e si azzerano al riavvio.

## Sviluppo

```bash
uv run pytest      # test con provider finti, senza rete
```

Il port Rust (`rust/src/`) ha gli stessi moduli, più `http.rs` e `util.rs`; i test d'integrazione sono in `rust/tests/routing.rs`.

Codice Python in `src/llm_router/`: `config.py` (impostazioni), `catalog.py` (scoperta e fusione dei modelli),
`normalize.py` (nomi), `scores.py` (punteggi), `router.py` (scelta e fallback), `health.py` (pause), `app.py` (HTTP).

## Licenza

MIT, vedi [LICENSE](LICENSE).
