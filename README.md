# freeway

Server locale **OpenAI-compatibile** che smista le richieste tra i provider LLM gratuiti
(OpenRouter, OpenCode Zen, Google AI Studio) e, se uno non risponde, passa al successivo.
Scegli un **livello di intelligenza** (`smart`, `balanced`, `fast`) oppure un **modello preciso**:
il router trova un provider che lo serva davvero.

Due versioni equivalenti, stesso `config.yaml` e stesso `.env`: **Python** (`src/`) e **Rust** (`rust/`, un solo binario, senza Python).
La Decision API, `GET /v1/models/{id}` e il modello `fake` esistono solo nella versione Rust.

---

# Uso

## Avvio

Serve prima `.env` con almeno un token e una chiave di provider (tabella sotto).

```bash
cp .env.example .env      # poi compilalo
uv sync && uv run llm-router                                  # Python: http://127.0.0.1:4100
cd rust && cargo build --release && cd .. && ./rust/target/release/llm-router   # Rust
```

Opzioni (uguali nelle due versioni): `--port 4200 --host 127.0.0.1 --config altro.yaml`.
`config.yaml` e `.env` si cercano nella cartella corrente (o `$ROUTER_CONFIG`): avvia dalla cartella del progetto.
Dopo ogni modifica a `config.yaml` o `.env` **riavvia**.

| Variabile in `.env` | A cosa serve | Dove si ottiene |
|---|---|---|
| `ROUTER_TOKENS` | password per chi chiama il router; **una o più, separate da virgola** | `python3 -c "import secrets; print(secrets.token_urlsafe(32))"` per ognuna |
| `OPENROUTER_API_KEY` | provider OpenRouter | openrouter.ai |
| `OPENCODE_API_KEY` | provider OpenCode Zen | opencode.ai |
| `GEMINI_API_KEY` | provider Google AI Studio | aistudio.google.com |
| `ARTIFICIAL_ANALYSIS_API_KEY` | punteggi di intelligenza dei modelli (senza, si usa l'ultima cache) | artificialanalysis.ai (gratis) |
| `LOG_LEVEL` | `INFO` (default) o `DEBUG` | — |

Un provider senza chiave viene **saltato** (lo dice `/admin/status`). Senza `ROUTER_TOKENS` il server è aperto a chiunque lo raggiunga.
Ogni token vale da solo: per dare accesso a un'app ne aggiungi uno, per toglierlo lo cancelli e riavvii. `.env` è nel `.gitignore`.

## Come si chiama

È un normale endpoint OpenAI: `base_url = http://127.0.0.1:4100/v1`, `api_key = <uno dei ROUTER_TOKENS>`
(`Authorization: Bearer ...`, oppure l'header `x-api-key`).

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

Funzionano streaming, tool calling e immagini, **se** il modello scelto li supporta (il router salta gli endpoint che dichiarano di no).
Esiste solo la chat: niente embeddings, niente formato Anthropic.

### Cosa mettere in `model`

| Valore | Cosa succede |
|---|---|
| `smart`, `balanced`, `fast` | **Tier**: prova i modelli migliori del tier, poi quelli dei tier di riserva |
| `gemini-3.8-flash` | **Sempre lo stesso modello**, su qualunque provider lo abbia. Non scala ad altri modelli |
| `openrouter/nvidia/nemotron-3.5-super-vl:free` | **Un solo endpoint** di un solo provider (`<provider>/<id del provider>`) |
| `fake` | Risposta **inventata in locale**, nessun provider contattato (solo Rust, vedi sotto) |

I nomi validi si leggono con `GET /v1/models`.

### Header della richiesta (opzionali)

| Header | Valori | Effetto |
|---|---|---|
| `x-router-fallback` | `auto` (default) | i tier scalano ai tier di riserva; un modello per nome non cambia modello |
| | `none` | resta nel tier/modello richiesto; se non c'è niente risponde 503 |
| | `tier` | anche un modello per nome, se fallisce, scala ai modelli del suo tier |
| `x-router-session` | un id qualsiasi | tiene la conversazione sullo stesso modello finché risponde (120 min) |

### Header della risposta: chi ha risposto

`x-router-model`, `x-router-tier` (o `unrated`), `x-router-provider`, `x-router-upstream-model`, `x-router-attempts` (tentativi, compreso quello riuscito).
Guardali: un tier può **declassare** in silenzio (`smart` può rispondere con un modello `balanced`). Per non declassare usa `x-router-fallback: none`.

### Ragionamento (reasoning)

Il router **inoltra il body così com'è**, quindi il ragionamento si controlla con il parametro standard OpenAI `reasoning_effort`, in ogni richiesta:

```python
client.chat.completions.create(model="fast", reasoning_effort="none", messages=[...])
```

- `"none"` è l'unico valore **verificato**: spegne il ragionamento su OpenRouter e su Google (un modello `fast`: ~9 s e ~390 token con ragionamento, meno di 1 s senza; `gemini-3.5-flash`: 5,9 s contro 2,0 s). Su OpenCode non è stato possibile provarlo (429).
- `"minimal"`, `"low"`, `"medium"`, `"high"` non sono stati provati; su OpenRouter `low` riduce il ragionamento ma non lo toglie.
- Senza il parametro ogni modello fa come vuole: è la scelta per i tier alti, dove il ragionamento serve.
- Non usare l'oggetto `reasoning: {...}` di OpenRouter: Google lo rifiuta con 400. Un modello che rifiuta il valore risponde 400: il router non lo mette in pausa e prova l'endpoint dopo.
- Non c'è ancora un default per tier in `config.yaml`.

### Errori

| Codice | Significato |
|---|---|
| 401 | token mancante o sbagliato |
| 404 | modello o tier sconosciuto (il messaggio elenca i tier) |
| 502 | tutti gli endpoint provati hanno fallito; `error.tried` dice cosa è successo a ciascuno |
| 503 `all_providers_busy` | tutto è in pausa; header `retry-after` con i secondi |
| 503 `no_endpoint` | nessun provider caricato, tier vuoto con `fallback: none`, o nessun modello supporta tool/immagini |

### Altri endpoint

Tutti richiedono il token, compreso `/health`.

| Endpoint | Cosa fa |
|---|---|
| `GET /v1/models` | tier e modelli con tier, punteggio e provider |
| `GET /v1/models/{id}` | un solo tier o modello (anche `<provider>/<id>`); 404 `model_not_found`. Solo Rust |
| `GET /admin/status` | provider attivi/saltati, punteggi, modelli per tier con pause ed errori, elenco `unrated` |
| `POST /admin/refresh` | rilegge subito gli elenchi dei provider |
| `GET /health` | `{"status":"ok"}` |
| `POST /v1/systemone` | Decision API, solo Rust (vedi sotto) |

---

# Dettagli

## Come decide

### 1. Quali modelli esistono
Ogni 30 minuti il router rilegge l'elenco modelli di ogni provider attivo e tiene solo quelli **gratuiti**.
OpenRouter e OpenCode indicano il prezzo (0); Google no, quindi lì si considera gratuito tutto ciò che è elencato
e si filtra con `exclude:` ciò che non serve (embeddings, immagini, modelli a pagamento).

### 2. Lo stesso modello su provider diversi
I nomi vengono normalizzati (tolti `:free`, `-free`, date, prefisso del produttore): `nvidia/nemotron-3.5-lightning:free`
e `nemotron-3.5-lightning-free` diventano un solo modello con due endpoint, ordinati per `priority` (più basso = prima).
Quello che non si aggancia da solo si dichiara in `aliases:`. Stesso modello non significa identico
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
Il piano gratuito di Artificial Analysis non pubblica l'indice di molti modelli. Un modello senza punteggio è **non valutato**:
nessun tier lo sceglie, ma si può chiamare per nome. `GET /admin/status` elenca gli `unrated`.
Per metterli nei tier dai un voto in `models:` (stessa scala). I voti in `config.yaml` marcati *ESTIMATES* sono stime, non misure.

### 5. Quando un provider non risponde

| Errore | Cosa fa il router |
|---|---|
| 429 (limite) | pausa dell'endpoint: usa `Retry-After`, altrimenti 60 s raddoppiando fino a 15 min. Dopo due 429 di modelli diversi dello stesso provider, pausa **dell'intero provider** |
| 401 (chiave) | pausa di tutto il provider per 10 min |
| 402 / 403 / 404 | pausa di quel modello per 30 min (es. «non disponibile nel tuo paese») |
| 5xx, timeout, risposte vuote | backoff da 20 s fino a 10 min |
| 400 (richiesta sbagliata) | nessuna pausa: se tutti rifiutano, l'errore originale torna al chiamante |

Anche errori nascosti in una risposta 200 vengono riconosciuti. In streaming il router aspetta il primo chunk valido prima di impegnarsi;
dopo il primo byte non può più cambiare provider. Si provano al massimo 8 endpoint per richiesta.
Le pause sono **in memoria**: se riavvii il server le dimentica.

## Modello `fake` (solo Rust)

`model: "fake"` risponde con testo **inventato in locale**: nessun provider, nessuna quota consumata. Serve per provare un client
(streaming, `usage`, header, timeout) senza chiavi né 429. Il testo dipende solo dall'ultimo messaggio utente (stesso messaggio, stessa risposta),
`max_tokens` lo accorcia con `finish_reason: "length"`. Non fa parte di nessun tier, quindi un fallback non lo sceglie mai;
si riconosce da `system_fingerprint: "fake"` e da `x-router-provider: fake`. Richiede comunque il token.

## Decision API (solo Rust)

`POST /v1/systemone` (alias `POST /api/alpha/decisions`, il nome usato da OpenRouter) risponde a **domande tipizzate** su uno `state`,
nello stesso formato dei modelli «System One» come Jev di TypeSafe: invece di testo restituisce probabilità.
`model` è un tier o un modello del router.

```bash
curl http://127.0.0.1:4100/v1/systemone -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d '{
  "model": "balanced",
  "state": "Mi avete addebitato due volte l'\''abbonamento. Rimborsatemi.",
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
| `score` | obbligatorio: array di 2–10 livelli, dal più basso al più alto | `score` (media pesata), `probabilities`, `legend`, `confidence` |

`state` può essere una stringa, un oggetto o un array; le immagini vanno nell'array come `{"type":"image_url","image_url":{"url":...}}`.
Non sono supportati `box` e `assets`. La risposta ha `model`, `answers` (stesse chiavi delle domande) e `usage`, più gli header `x-router-*`.

**Attenzione:** le risposte vengono dai modelli di chat gratuiti, ai quali si chiede un JSON. Le probabilità sono stime del modello, **non calibrate**
come quelle di Jev, e `confidence` la calcola il router dalla dispersione (1 = tutta su un'opzione, 0 = uniforme).
Se un modello risponde con qualcosa di inutilizzabile viene messo in pausa e se ne prova un altro (fino a 3). I modelli «ragionanti» del tier `fast`
possono impiegare decine di secondi: per decisioni rapide conviene un modello non ragionante, per nome. Modelli a pagamento come Jev o Clef non sono supportati.

## Configurazione (`config.yaml`)

- `tiers`: nomi, soglie e riserve dei livelli.
- `providers`: per ognuno `base_url`, `api_key_env`, `priority`, `include`/`exclude` (glob sugli id del provider), `enabled`.
  Per aggiungerne uno compatibile OpenAI (Groq, Cerebras, NVIDIA...) basta un blocco con `kind: openai`, `base_url` e `api_key_env`.
- `models`: correzioni tue. `score` (ricalcola il tier), `tier` (forza), `disabled: true` (esclude).
  La chiave può essere qualunque nome con cui il modello compare in `/v1/models`.
- `aliases`: nomi da trattare come lo stesso modello. `exclude`: glob sui nomi da escludere ovunque.
- Tempi: `first_token_timeout`, `stream_idle_timeout`, `request_timeout`, `max_attempts`, `refresh_minutes`.

Un tier si può anche creare a mano: con `min_score: 101` nessun modello ci arriva da solo, e lo riempi con `models: { nome: { tier: mio-tier } }`.

## Stato attuale e limiti

- **OpenCode** risponde 429 «Free usage limit reached» a tutti i suoi modelli gratuiti, anche con chiave valida; il router lo mette in pausa e riprova. Finché dura, `smart` risponde da Google.
- **`inkling-2-small`** è disabilitato: 403 «non disponibile nel tuo paese» su ogni provider.
- **OpenRouter** nasconde alcuni modelli gratuiti (i Nemotron di NVIDIA, Liquid) se nelle impostazioni di privacy dell'account non permetti i modelli che possono registrare i prompt.
  Sui modelli gratuiti i prompt possono essere registrati dal provider: non mandare dati sensibili.
- **Google**: i Pro, `omni`, `gemini-4-*` non hanno quota gratuita; i `gemini-2.5-*` sono chiusi ai nuovi utenti.
- I limiti gratuiti cambiano spesso: il catalogo si riallinea da solo. Il router non conosce le quote reali: le scopre dai 429.

## Sviluppo

```bash
uv run pytest            # Python, provider finti, senza rete
cd rust && cargo test    # Rust: unit test e integrazione (rust/tests/routing.rs)
```

Python in `src/llm_router/`: `config.py`, `catalog.py` (scoperta e fusione dei modelli), `normalize.py`, `scores.py`, `router.py` (scelta e fallback), `health.py` (pause), `app.py` (HTTP).
Rust in `rust/src/`: gli stessi moduli, più `http.rs`, `util.rs`, `systemone.rs` (Decision API) e `fake.rs`.

---

# Raspberry Pi (minimale)

Serve un Pi con Linux a 64 bit (`uname -m` → `aarch64`). La build è lenta ma si fa una volta; la cross-compilazione dal PC non è stata provata.

```bash
# sul Pi
sudo apt install -y build-essential git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh        # poi riapri la shell
git clone https://github.com/GabrieleGhisleni/freeway.git ~/freeway && cd ~/freeway
(cd rust && cargo build --release)
cp .env.example .env && chmod 600 .env && nano .env && mkdir -p data   # chiavi e ROUTER_TOKENS
sudo cp deploy/llm-router-rust.service /etc/systemd/system/llm-router.service   # controlla User e percorsi /home/pi/freeway
sudo systemctl daemon-reload && sudo systemctl enable --now llm-router
curl -H "Authorization: Bearer <token>" http://127.0.0.1:4100/health
```

Il servizio ascolta su `0.0.0.0`: imposta sempre `ROUTER_TOKENS` e non esporlo su internet (rete di casa o VPN come Tailscale). I client usano `base_url = http://<ip-del-pi>:4100/v1`.
Log: `journalctl -u llm-router -f`. Aggiornamento: `git pull && (cd rust && cargo build --release) && sudo systemctl restart llm-router`.

---

MIT, vedi [LICENSE](LICENSE).
