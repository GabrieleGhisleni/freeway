# Usare freeway

freeway è un endpoint OpenAI-compatibile (solo chat). Impostazioni per qualsiasi client:

| | |
|---|---|
| `base_url` | `https://freeway.gabrieleghisleni.dev/v1` (in locale: `http://127.0.0.1:4100/v1`) |
| `api_key` | uno dei `ROUTER_TOKENS` configurati sul Pi |
| `model` | `smart`, `balanced`, `fast` oppure un modello preciso (vedi sotto) |

Il token si chiede a chi gestisce il server e **non va mai scritto nel repo**: negli esempi è `$TOKEN`.
Se ne gestisci il Pi: `deploy/env.sh new-token` ne crea uno nuovo (vedi [deploy.md](deploy.md)).

Prova veloce, senza consumare quote dei provider (`fake` risponde con testo inventato in locale):

```bash
curl https://freeway.gabrieleghisleni.dev/v1/chat/completions -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"model":"fake","messages":[{"role":"user","content":"Ciao!"}]}'
```

## Come si chiama

È un normale endpoint OpenAI: `base_url = https://freeway.gabrieleghisleni.dev/v1`, `api_key = <uno dei ROUTER_TOKENS>`
(`Authorization: Bearer ...`, oppure l'header `x-api-key`).

```bash
curl https://freeway.gabrieleghisleni.dev/v1/chat/completions \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"model":"smart","messages":[{"role":"user","content":"Ciao!"}]}'
```

```python
from openai import OpenAI

client = OpenAI(base_url="https://freeway.gabrieleghisleni.dev/v1", api_key="<uno dei ROUTER_TOKENS>")
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
| `fake` | Risposta **inventata in locale**, nessun provider contattato (solo Rust, vedi [README](../README.md#modello-fake-solo-rust)) |

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
| 503 `all_providers_failed` | tutti gli endpoint provati hanno fallito; il messaggio li riassume per tipo (es. `5 rate_limit, 2 server`), `error.tried` dice cosa è successo a ciascuno, `retry-after` indica quando uno si libera |
| 503 `all_providers_busy` | tutto è in pausa; header `retry-after` con i secondi |
| 503 `no_endpoint` | nessun provider caricato, tier vuoto con `fallback: none`, o nessun modello supporta tool/immagini |

Un errore del router è sempre un JSON con `error.type`. Non usa mai 502 né 504: Cloudflare, davanti al server, li sostituisce con una sua pagina HTML.

### Altri endpoint

Tutti richiedono il token, compreso `/health`.

| Endpoint | Cosa fa |
|---|---|
| `GET /v1/models` | tier e modelli con tier, punteggio e provider |
| `GET /v1/models/{id}` | un solo tier o modello (anche `<provider>/<id>`); 404 `model_not_found`. Solo Rust |
| `GET /admin/status` (*) | provider attivi/saltati, punteggi, modelli per tier con pause ed errori, elenco `unrated` |
| `POST /admin/refresh` (*) | rilegge subito gli elenchi dei provider |
| `GET /health` | `{"status":"ok"}` |
| `POST /v1/systemone` | Decision API, solo Rust (vedi [README](../README.md#decision-api-solo-rust)) |

(*) Da internet rispondono 404: il tunnel li blocca. Si usano dal Pi: `ssh rpi 'curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:4100/admin/status'`.
