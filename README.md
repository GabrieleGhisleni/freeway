# freeway

Server locale **OpenAI-compatibile** che smista le richieste tra i provider LLM gratuiti
(OpenRouter, OpenCode Zen, Google AI Studio) e, se uno non risponde, passa al successivo.
Scegli un **livello di intelligenza** (`smart`, `balanced`, `fast`) oppure un **modello preciso**:
il router trova un provider che lo serva davvero.

Un solo binario Rust (`rust/`), configurato da `config.yaml` e `.env`. La vecchia versione Python è stata rimossa: se mai tornasse, deve restare *feature parity* con Rust.

**Documentazione**

- [docs/uso.md](docs/uso.md): come chiamare il servizio (URL, token, `model`, header, errori, endpoint).
- [docs/deploy.md](docs/deploy.md): Raspberry Pi, Cloudflare Tunnel, `deploy/deploy.sh` e gestione dei secrets.
- Più sotto: come il router sceglie i modelli, `config.yaml`, modello `fake`, Decision API.

---

# Avvio locale

Serve prima `.env` con almeno un token e una chiave di provider (tabella sotto).

```bash
cp .env.example .env      # poi compilalo
cd rust && cargo build --release && cd .. && ./rust/target/release/freeway   # http://127.0.0.1:4100
```

Opzioni: `--port 4200 --host 127.0.0.1 --config altro.yaml`.
`config.yaml` e `.env` si cercano nella cartella corrente (o `$ROUTER_CONFIG`): avvia dalla cartella del progetto.
Dopo ogni modifica a `config.yaml` o `.env` **riavvia**.

| Variabile in `.env` | A cosa serve | Dove si ottiene |
|---|---|---|
| `ROUTER_TOKENS` | password per chi chiama il router; **una o più, separate da virgola** | `openssl rand -base64 32` per ognuna |
| `OPENROUTER_API_KEY` | provider OpenRouter | openrouter.ai |
| `OPENCODE_API_KEY` | provider OpenCode Zen | opencode.ai |
| `GEMINI_API_KEY` | provider Google AI Studio | aistudio.google.com |
| `NVIDIA_API_KEY` | provider NVIDIA (build.nvidia.com) | build.nvidia.com |
| `GROQ_API_KEY` | provider Groq | console.groq.com |
| `ZAI_API_KEY` | provider Z.ai (GLM) | z.ai |
| `ARTIFICIAL_ANALYSIS_API_KEY` | punteggi di intelligenza dei modelli (senza, si usa l'ultima cache) | artificialanalysis.ai (gratis) |
| `LOG_LEVEL` | `INFO` (default) o `DEBUG` | — |

Un provider senza chiave viene **saltato** (lo dice `/admin/status`). Senza `ROUTER_TOKENS` il server è aperto a chiunque lo raggiunga.
Ogni token vale da solo: per dare accesso a un'app ne aggiungi uno, per toglierlo lo cancelli e riavvii. `.env` è nel `.gitignore`.

# Deploy su Raspberry Pi

```bash
deploy/deploy.sh                  # test + build aarch64 (Docker) + upload + restart sul Pi
deploy/deploy.sh --no-build       # riusa l'ultima build; altre opzioni: --skip-tests, --force
deploy/env.sh set GEMINI_API_KEY  # chiavi e token: solo sul Pi, mai da git (anche: keys, new-token, edit)
```

`deploy.sh` compila sul PC e carica sul Pi solo il binario e `config.yaml`; il `.env` non lo tocca. Rifiuta di partire se non sei su `main` o se ci sono modifiche non committate.
Sul Pi basta una cartella con `freeway`, `config.yaml`, `.env` e `data/`; il server è esposto con un Cloudflare Tunnel. Dettagli, prima installazione e operazioni: [docs/deploy.md](docs/deploy.md).

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
nessun tier lo sceglie *finché c'è un modello valutato disponibile*; si può sempre chiamare per nome.
Quando tutti gli endpoint valutati sono in pausa o falliscono, il router prova i non valutati (in ordine alfabetico) **prima di rispondere con un errore**; con `x-router-fallback: none` questo non succede.
`GET /admin/status` elenca gli `unrated`; la risposta ha `x-router-tier: unrated`.
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

## Modello `fake`

`model: "fake"` risponde con testo **inventato in locale**: nessun provider, nessuna quota consumata. Serve per provare un client
(streaming, `usage`, header, timeout) senza chiavi né 429. Il testo dipende solo dall'ultimo messaggio utente (stesso messaggio, stessa risposta),
`max_tokens` lo accorcia con `finish_reason: "length"`. Non fa parte di nessun tier, quindi un fallback non lo sceglie mai;
si riconosce da `system_fingerprint: "fake"` e da `x-router-provider: fake`. Richiede comunque il token.

## Decision API

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
  Per aggiungerne uno compatibile OpenAI (Cerebras, Mistral...) basta un blocco con `kind: openai`, `base_url` e `api_key_env`.
  Come si scoprono i modelli gratis dipende dal `kind`: `openrouter` e `opencode` leggono il prezzo dall'elenco; `zai` legge la
  [pagina prezzi](https://docs.z.ai/guides/overview/pricing) (il suo `/models` non dice nulla e non elenca i `-flash` gratuiti);
  `openai` prende tutto ciò che l'elenco riporta.
  Con `probe: true` Freeway manda a ogni modello elencato una chat da 1 token e tiene solo quelli che rispondono: serve quando l'elenco
  è pieno di modelli morti o non-chat (NVIDIA ne elenca 64, ne rispondono circa 9). Le risposte si ricordano (sì 24 h, no 6 h, incerto 10 min),
  quindi dopo l'avvio ogni refresh interroga solo i modelli nuovi. L'avvio è più lento di qualche decina di secondi.
  I modelli si ricontrollano da soli ogni `refresh_minutes`, senza bisogno di deploy.
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
cd rust && cargo test    # unit test e integrazione con provider finti (rust/tests/routing.rs), senza rete
```

Codice in `rust/src/`: `config.rs`, `catalog.rs` (scoperta e fusione dei modelli), `normalize.rs`, `scores.rs`, `router.rs` (scelta e fallback), `health.rs` (pause e contatori, salvati in `data/health.json`), `app.rs` (HTTP), `http.rs`, `util.rs`, `systemone.rs` (Decision API) e `fake.rs`.

---

MIT, vedi [LICENSE](LICENSE).
