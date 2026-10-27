# DSP fondamentale — la teoria minima per questo progetto

Scritto per chi sa programmare bene (Java/JS/Python) e di audio ha l'intuizione, non la
matematica. Ogni sezione termina con **"dove vive nel nostro codice"**, così la teoria
non resta astratta. Ordine di lettura: dall'inizio. I §4-5 sono il cuore del progetto.

---

## 1. Tutto è un array di numeri

L'audio digitale è una sequenza di numeri (`sample`), uno per istante di tempo. La
**sample rate** (`fs`) dice quanti ce ne sono in un secondo: 48 000 Hz = 48 000 numeri
al secondo per ogni canale. Un **frame** è l'insieme dei sample di tutti i canali nello
stesso istante (stereo: 2 sample per frame).

Il driver audio non ti consegna i sample uno a uno: ti consegna **blocchi**. A 48 kHz con
buffer da 64 sample hai un blocco ogni **64/48000 s = 1,33 ms**. Tutto il tuo DSP deve
stare dentro quel tempo, ogni blocco, per sempre. Questo è il "budget", ed è il motivo
per cui il §10 esiste.

**Nyquist**: con fs=48 kHz puoi rappresentare frequenze fino a 24 kHz, non di più. Una
sinusoide a 30 kHz campionata a 48 kHz non sparisce: **riappare a 18 kHz** (si "ripiega":
aliasing). È irriversibile — una volta aliassato, quel segnale è corrotto per sempre.
Tutto ciò che genera nuove frequenze (distorsione, ring modulation, pitch shift) deve
quindi gestire l'aliasing: vedi §8.

> **Dove vive**: `hathor_dsp_core::buffer` (il blocco), `ProcessCtx::block_budget_us()` (il
> budget), e ogni filtro che progetta `f0 < Nyquist` (`coeffs::design` lo *rifiuta*
> sopra Nyquist — test `invalid_inputs_are_rejected`).

## 2. Decibel: l'unica unità che conta

L'orecchio risponde ai **rapporti**, non alle differenze. Per questo si usano i logaritmi:

```
dB = 20 · log10(ampiezza₂ / ampiezza₁)
```

Numeri da sapere a memoria:

| Δ dB | Rapporto | Significato pratico |
|---|---|---|
| +6 | ×2 | "il doppio del volume" (di ampiezza) |
| +12 | ×4 | raddoppio del raddoppio |
| +20 | ×10 | un ordine di grandezza |
| −6 | ×0.5 | metà ampiezza |
| −inf | ×0 | silenzio |
| +1..3 | ~×1.15-1.4 | "si sente appena" — la risoluzione utile di un fader |

**dBFS** = dB *Full Scale*: 0 dBFS è il massimo numerico rappresentato (±1.0 in float),
e tutto il resto è negativo. Attenzione al trabocchetto: in float 32 **puoi** superare
0 dBFS (il numero esiste), ma il convertitore D/A no — quello che supera 0 dBFS clippa.
Da qui il **true peak** del §9.

Il **gain staging** è tutto qui: ogni stadio guadagna/perde dB; se arrivi al master con
i picchi a −0.1 dBFS non hai headroom per lavorare, se ci arrivi a −40 dBFS stai
spreco risoluzione. Un mixer è fondamentalmente una macchina per gestire dB.

> **Dove vive**: `coeffs::db_to_lin` / `lin_to_db`; tutti i parametri "gain" del progetto
> sono in dB nell'UI e lineari nel DSP.

## 3. Filtri: FIR e IIR

Un filtro modifica lo spettro: attenua/enfatizza bande di frequenza.

**FIR** (Finite Impulse Response): `y[n] = Σ bₖ·x[n−k]`. Solo feed-forward.
- Pro: può essere **linear phase** (vedi §5), stabilità garantita.
- Contro: per pendenze ripide a bassa frequenza servono centinaia di coefficienti →
  costo e **latenza** (un FIR di N coefficienti in forma simmetrica ritarda di N/2).

**IIR** (Infinite Impulse Response): `y[n] = Σ bₖ·x[n−k] − Σ aₖ·y[n−k]`. Ha feedback:
l'uscita rientra nell'ingresso.
- Pro: pendenze ripide con 5 coefficienti (il **biquad**, ordine 2), latenza zero.
- Contro: può **diventare instabile** (i "poli" escono dal cerchio unitario e l'uscita
  esplode), e la fase è stortata (minimum-phase).

Il **biquad** è l'atomo di ogni EQ: 2 poli + 2 zeri, 5 coefficienti. Un EQ a 8 bande è
8 biquad in cascata. Le tipologie (low/high pass, band, notch, peak/campana, shelf,
allpass) sono solo scelte diverse dei coefficienti — le formule standard sono quelle del
**cookbook di RBJ**, che normalizzano tutto a `a0 = 1`.

**Poli e zeri** (intuizione, senza matematica): gli zeri sono frequenze che il filtro
*cancella*, i poli quelle che *amplifica/risuona*. Q alto = polo vicino al cerchio
unitario = risonanza stretta e pronunciata. Q altissimo + polo fuori dal cerchio =
esplosione. Il criterio di stabilità per l'ordine 2 è `|a2| < 1` e `|a1| < 1 + a2`.

> **Dove vive**: `hathor_dsp_filters::coeffs` (design RBJ + normalizzazione a0 + criterio di
> stabilità), `hathor_dsp_filters::biquad` (la ricorrenza).

## 4. La ricorrenza: DF1 vs DF2T, e perché i coefficienti si rampano

Il biquad si implementa in due forme equivalenti in matematica, **non** equivalenti in
virgola mobile:

```
DF1:  y = b0x + b1x⁻¹ + b2x⁻² − a1y⁻¹ − a2y⁻²      (ritardi espliciti su x e y)
DF2T: out = b0·x + z1
      z1  = b1·x − a1·out + z2
      z2  = b2·x − a2·out                            (stato trasposto)
```

La **DF2T** (trasposta) è lo standard nei plugin professionali: un solo punto di
arrotondamento per ramo, e l'errore non si accumula sui poli vicini all'unità come nella
DF1. È anche la forma che si vectorizza fra canali (§ "SIMD" sotto).

**Attenzione al termine `− a2·out` in z2**: è facile dimenticarlo (è successo davvero in
questo repo, commit history docet). Il filtro risultante resta stabile e quasi giusto in
magnitudine, ma **sbaglia la fase** — e in un tool che allinea fasi, un bug di fase è il
peggiore possibile. Morale: i test devono confrontare il comportamento
*esterno* (risposta misurata) con la teoria (risposta predetta), non due implementazioni
scritte dalla stessa testa.

**Zipper noise e ramping**: se cambi il cutoff di 1 kHz ogni blocco (750 volte al
secondo), il filtro "scatta" e senti un ronzio. La soluzione ingenua — ricalcolare i
coefficienti per ogni sample — costa `sin/cos/sqrt` a sample (~50 ns × 64 ≈ 3 µs per
filtro per blocco: da solo è ~0.2–0.3% del budget di 1333 µs, ma si moltiplica per
bande × canali e trascina i trascendenti dentro la callback). La soluzione giusta:
**rampare linearmente i 5 coefficienti**
dal vecchio set al nuovo attraverso il blocco. Costa 5 addizioni per sample e il filtro
non vede mai gradini.

**SIMD, e la verità sui filtri**: la ricorrenza `out → z1 → out'` è una catena di
dipendenza *per sample*: non puoi calcolare il sample 5 prima del 4. Quindi **un biquad
mono non si vectorizza lungo il tempo**. Si vectorizza **fra canali**: 8 canali = 8
ricorrenze indipendenti = 8 lane di un registro AVX2. Ma questo pretende un layout
diverso (interleaved-by-channel) e la conversione di layout ha un costo, che va misurato
(non nascosto).

> **Dove vive**: `Biquad::process_sample` (DF2T), `Biquad::ramp_to` (ramping),
> `lanes8::BiquadLanes8` (AVX2 8 canali), test `measured_response_matches_predicted_response`.

## 5. Fase: la grandezza che ogni allineamento deve rispettare

Ogni filtro modifica l'ampiezza **e la fase** di ogni frequenza. La fase dice *quando*,
all'interno del periodo, avviene il picco dell'onda.

**Il conto fondamentale** (memorizzalo): una sinusoide a frequenza `f` compie 360° in
`1/f` secondi. Quindi un ritardo di `Δt` secondi = sfasamento di:

```
φ = 360° · f · Δt        →   a 50 Hz, 1 ms = 18°
                            a 100 Hz, 1 ms = 36°
                            a 1 kHz, 1 ms = 360° (= zero, aliasing di fase!)
```

Conseguenze che guidano tutto il percorso di allineamento di fase:

1. **"Spostare di 1 ms" non è un'operazione a fase costante**: è 18° a 50 Hz e 360° a
   1 kHz. Un nudge temporale allinea il transiente ma può *peggiorare* il fondamentale.
   Ecco perché il tool deve ragionare in sample (1 sample a 48k = 20.8 µs = 0.375° a 50 Hz)
   e per banda.
2. **Sotto ~80 Hz la "fase" che senti è l'allineamento del fondamentale**, non del
   transiente. Kick e basso che "punchano" insieme hanno i fondamentali in fase; se sono
   in opposizione (180°) le due onde si **cancellano** in somma → il mix suona vuoto
   proprio dove dovrebbe spingere. È il problema nº1 del mix low-end.
3. **Polarità ≠ fase**: invertire la polarità (×−1) è 180° *a tutte le frequenze*
   insieme. La fase di un filtro varia con la frequenza. Un tasto "polarity flip" non
   risolve un problema di fase, e viceversa.
4. **Group delay** = quanto ritarda una banda: `τ(f) = −dφ/dω`. Un EQ minimum-phase ha
   group delay concentrato vicino al cut (suono "naturale", transienti integri); un
   linear-phase EQ ha group delay **costante** (zero distorsione di fase, ma
   **pre-ringing** sui transienti — il colpo di cassa "sbava" indietro nel tempo — e
   latenza di N/2 sample da compensare). Nessuno dei due è "giusto": sono trade-off.
5. **L'allpass è il bisturi della fase**: magnitudine piatta, ruota solo la fase. Metti
   in cascata allpass a frequenze diverse e costruisci una curva di fase arbitraria
   *senza toccare lo spettro*. È il "phase rotator per banda" del plugin kick↔bass.

**Come si misura**: la correlazione fra due segnali (−1..+1) dice quanto sono "in fase"
in banda larga (il meter stereo classico); il **null test** (somma di un segnale con la
copia invertita dell'altro: ciò che resta = la differenza) è la misura più onesta che
esista di "cosa ho cambiato davvero"; la cross-correlazione in funzione del lag trova il
ritardo ottimo fra due segnali (allineamento kick transient).

> **Dove vive**: `Coeffs::phase_rad` (fase predetta), test comportamentale di fase,
> `hathor-dsp-measure` (correlazione, null test, lag) → M0-M3.

## 6. FFT: dallo spettro all'analyzer

La FFT trasforma N sample nel tempo in N/2+1 **bin** di frequenza: il bin `k` rappresenta
la frequenza `k·fs/N`. Due leggi da interiorizzare:

1. **Risoluzione in frequenza = fs/N**. Con N=16384 a 48k: 2,9 Hz per bin. Con N=1024:
   47 Hz. Non esiste "alta risoluzione e veloce insieme": N grande = finestra lunga =
   aggiornamento lento e dettaglio temporale perso.
2. **Finestra e leakage**: la FFT assume che il blocco sia periodico. Un segnale che non
   "chiude" ai bordi spalma energia sui bin vicini (leakage). Si applica una **finestra**
   (Hann: `0.5·(1−cos)`) che porta a zero i bordi, al costo di allargare i picchi.
   Senza finestra, uno spectrum analyzer mente.

**STFT** = FFT ripetuta su finestre sovrapposte (hop = N/2 o N/4): è così che funziona
ogni analyzer. Il costo è N·logN per blocco; a 16384 sono ~230k operazioni — fattibile
ma non gratis, e su un mix a 40 tracce non lo paghi su ogni traccia.

**Il trucco di SPAN** (e il progetto di `hathor-dsp-fft` in M1): invece di UNA FFT enorme, un
**banco multi-risoluzione**: FFT piccole ad alto rate per le alte frequenze (dove
l'orecchio vuole reattività), FFT grandi a rate basso per le basse (dove serve
risoluzione: a 50 Hz, 47 Hz/bin non distingue niente). Più smoothing in dominio
magnitudine e "freeze/hold". La fisica è la stessa; l'architettura è la qualità.

**dB per bin**: raddoppi N e ogni bin ha metà energia → la traccia scende di 3 dB. Un
analyzer serio normalizza per larghezza di banda, altrimenti "la stessa canzone" mostra
livelli diversi al cambiare della risoluzione.

> **Dove vive**: `hathor-dsp-fft` (M0: radix-2 + real-FFT via packing; M1: banco multi-rate).
> La real-FFT via packing (due reali in un complesso, FFT di metà lunghezza) è il primo
> risparmio del 2× che imparerai.

## 7. Dinamica: compressori senza magia

Un compressore è: **detector** (misura il livello: peak o RMS, con attack/release) →
**curva** (sotto soglia: 1:1; sopra: ratio, es. 4:1 = 4 dB in ingresso → 1 dB in uscita)
→ **gain applicato** (il gain varia nel tempo: è l'inviluppo).

Concetti che fanno la differenza fra "compressore" e "compressore buono":

- **Feedforward** (detector sull'ingresso) vs **feedback** (detector sull'uscita): il
  feedback suona più "musicale" (il classico LA-2A) ma è più lento a reagire.
- **Lookahead**: il gain agisce *prima* che il picco arrivi (delay line sul segnale di
  N sample = latenza dichiarata, vedi §5/architecture PDC). Senza lookahead, i transienti
  passano sempre ("punch" preservato ma ceiling non garantito).
- **Knee**: transizione dolce intorno alla soglia (hard knee = il gain "scatta").
- **Key filtering / sidechain**: il detector può guardare un segnale *filtrato* (HPF a
  100 Hz: la cassa non pompa il compressore del mix con la sua energia LF) o un segnale
  *esterno* (il kick ducka il basso = sidechain classico).
- **Multibanda**: N crossover + N compressori + ricomposizione. Il crossover introduce
  sfasamento fra bande (Linkwitz-Riley: −360° a coppia, che è "in fase" in somma; i
  crossover a fase lineare esistono ma costano latenza). Se le bande pompano in modo
  indipendente, il timbro cambia mentre comprimi: è il difetto dei multibanda economici.
- **Pumping/breathing**: gli artefatti *musicali* del gain che respira. Non sono bug:
  sono il suono della sidechain EDM.

> **Dove vive**: `hathor-dsp-dyn` (M4). Il detector env-follower one-pole è già in
> `hathor_dsp_core::param::SmoothedParam` (stessa matematica, uso diverso).

## 8. Distorsione: saturazione e oversampling

Un saturatore è una funzione nonlineare `y = f(x)` applicata sample per sample (tanh,
cubica, waveshaper a tratti). Due cose da capire:

1. **Armoniche**: una nonlinearità genera multiple della frequenza originale (2f, 3f...).
   Curve **pari-simmetriche** → armoniche pari (2f, 4f: "caldo", "valvolare");
   **dispari** → dispari (3f, 5f: "aggressivo", "transformer"). Il carattere di un
   saturatore È la forma della sua curva.
2. **Aliasing**: se 3f supera Nyquist, ripiega in banda (§1) come rumore inarmonico
   metallico. La cura è l'**oversampling**: si interpola ×2/×4 (polifase), si applica la
   nonlinearità al rate alto (dove le armoniche "stanno dentro"), si filtra
   l'anti-immagine e si decima. Costa CPU proporzionale al fattore: ×4 su un distortore
   stereo = 8× il lavoro.

> **Dove vive**: `hathor-dsp-distort` (M4) — ma i filtri polyphase che servono sono cugini di
> quelli di `hathor-dsp-fft` (M1): stessa famiglia di primitive.

## 9. Loudness e mastering: i numeri veri

- **RMS** ≈ energia media; **peak** = il massimo; **LUFS** (ITU-R BS.1770) = loudness
  percepita con pesatura K (una shelf HF +6... no: un high-shelf +4 dB e un HPF), somma
  dei canali con pesi (L+R+1.41·surround), gating a −10 LU relativi. È il numero con cui
  Spotify/YouTube normalizzano (−14 LUFS tipici). Si misura, non si stima.
- **True peak**: due sample a −1.0 dBFS possono produrre un picco *analogico* sopra 0
  fra un sample e l'altro (l'interpolazione del DAC). I limiter mastering guardano il
  true peak (oversample ×4 + max), non il sample peak.
- **Dither**: quantizzare (es. 24→16 bit) aggiunge rumore correlato al segnale; sommare
  rumore bianco a −90 dB *prima* di quantizzare decorrela (TPDF). Il noise shaping lo
  sposta dove l'orecchio sente meno.

> **Dove vive**: `hathor-dsp-measure` loudness (M4), limiter true-peak (M4).

## 10. Igiene numerica: le cose che mordono in produzione

- **Denormal**: float minuscoli (< ~1.2e−38) che su x86 costano 10-100× un float normale.
  Un feedback IIR che decade verso zero ci finisce *sempre*. Cura: FTZ/DAZ nel registro
  MXCSR del thread audio (bit 15 + bit 6). È una riga, e senza quella riga il progetto
  "rallenta misteriosamente dopo 20 minuti".
- **f32 vs f64**: il processing è f32 (velocità, SIMD larga il doppio); gli accumulatori
  di metering/FFT lunghi e i coefficienti di progetto si calcolano in f64 e si convertono
  una volta. Sommare 10⁶ valori in f32 perde precisione in modo visibile.
- **Cancellazione catastrofica**: sommare numeri grandi che si annullano (i coefficienti
  di uno shelf a DC) perde cifre. I test valutano gli shelf a 20 Hz, non a 0 Hz, per
  questo.
- **NaN**: si propagano in silenzio attraverso tutto il grafo e uccidono l'output.
  Watchdog per nodo (1 sample su 64 controllato) → nodo mutato + report, mai NaN nel mix.

> **Dove vive**: `hathor_dsp_core::denormal`, note numeriche sparse nei test.

---

## Mappa "desiderio → capitolo"

| Vuoi... | Devi padroneggiare |
|---|---|
| EQ/Filter | §3, §4 |
| Kick/bass phase tool | **§5** (+ §6 per il masking meter, §7 per la sidechain) |
| SPAN-like | §6 |
| Compressor/Multiband | §7 |
| Saturator | §8 |
| Mastering suite | §9 (+§5 per il linear-phase) |
| Synth (M8) | §1 (aliasing!), §8 (waveshaping), §3 (SVF/ladder filters) |
| Reverb (M4) | §4 (allpass!), delay lines, FDN — capitolo dedicato in M4 |

## Esercizi (fai questi prima di scrivere il prossimo modulo)

1. Con `Coeffs::magnitude_db`: disegna (stampa) la curva di un lowpass Q=0.707 a 1 kHz;
   poi Q=8; trova dove il picco supera 0 dB e di quanto. Deve coincidere con ~20log10(Q).
2. Con `measure_steady_state` (è nei test): misura il group delay *effettivo* del lowpass
   a 100 Hz (fase/ω) e confrontalo con 1 sample. Capirai perché "fase" e "ritardo" sono
   la stessa moneta.
3. Calcola: quanti gradi di sfasamento a 60 Hz introducono 3 sample a 48 kHz?
   (risposta: 1,35° — e ora sai perché il nudge del plugin kick↔bass è in sample, non in ms.)
4. Genera un'onda quadra a 1 kHz, applica `y = tanh(3x)`, e guarda quanta energia c'è
   sopra 24 kHz. Quello è l'aliasing del §8, misurato con le tue mani.
