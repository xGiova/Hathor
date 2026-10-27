# Reverse engineering, prior art, e cosa studiare invece

La domanda era: *"se facessimo reverse engineering di una DAW, avremmo benefici nella
qualità del codice?"*

Risposta breve: **no sul binario chiuso; sì enorme se ridefinisci cosa intendi per
"reverse engineering"**. Le tre attività che ti danno il 95% del beneficio non toccano
un solo disassembler. E una di queste tre diventa un *prodotto* del tuo progetto, non un
mezzo.

---

## 1. Perché il RE dei binari non ti dà qualità di codice

### Non è la fonte della qualità

La qualità, in una DAW, sta in quattro cose che un `.exe` non ti mostra:

1. **il modello di dati** (come Region/Clip/Take/Automation sono correlati) — è
   *decisione*, non codice; dal binario vedi la disposizione dei campi, non il perché;
2. **il confine di threading** (chi possiede cosa, cosa attraversa il confine audio) —
   dai simboli ottimizzati non distinguibili dal rumore;
3. **la costellazione di edge case** gestiti (sample-rate change a metà recording, un
   plugin che cambia latenza in corsa, time signature dentro una region);
4. **la cura numerica** (perché DF2T e non DF1, perché il coefficient ramping) — questi
   sono *numeri*, e i numeri in un binario Release sono costanti moltiplicate in
   registri, senza unità né contesto.

Viceversa, quello che il RE ti mostra bene (struct layout, ordine dei loop, uso di SSE)
è la parte **meno** trasferibile: copiarlo fuori contesto produce codice che *sembra*
ottimizzato e non lo è.

### Il costo reale, per uno solo

`ida`/`ghidra` su un PE da 80 MB con C++ + vtable + template: settimane prima del primo
risultato interpretabile. IlDSP dentro una DAW commerciale frazionario è spesso scritto a
mano in assembly per AVX2 (perché loro possono permettersi di mantenere quell'assembly,
tu no). Il rapporto valore/costo è il peggiore possibile nel tuo caso.

### Il rischio legale, che è la ragione vera per non farlo

* **EULA**: Ableton, Avid, PreSonus proibiscono esplicitamente il reverse engineering.
  Una clausola contrattuale ti vincola *anche quando* la legge ti proteggerebbe.
* **Legge** (sei in Italia → UE): la direttiva 91/256/EEC art. 6 consente la
  decompilazione **solo** per ottenere le informazioni necessarie all'**interoperabilità**,
  solo sulle parti necessarie, e **non** per creare un'opera sostanzialmente simile.
  Tradotto: RE per far parlare il *tuo plugin* con Ableton → probabilmente coperto.
  RE per copiare il *funzionamento* di Ableton → **non** coperto.
* **Derivative work**: una volta che hai letto i loro interne, la tua "clean room" non è
  più pulita. Per un progetto open source che un giorno potrebbe avere una licenza
  commerciale, questo è un rischio che si *trasferisce* agli utenti e ai contributori.
* **UI/trade dress**: il look è la parte più protetta e la più facile da "copiare" senza
  volerlo. Le *concetti* di workflow (scorciatoie non-modali, modello a clip) non sono
  proteggibili; la loro resa visiva sì.

---

## 2. Le tre attività che ti conviene fare davvero

### A. Analisi **black-box comportamentale** ← la più preziosa, e diventa un prodotto

Non guardi dentro. **Misuri fuori.** Dai in pasto un segnale noto e registri cosa esce.
Con una catena di test automatizzata scopri i *design target* di qualsiasi plugin, senza
leggere una riga di assembly:

| Stimolo | Cosa misuri | Cosa ti insegna |
|---|---|---|
| Swept sine + deconvoluzione | **impulse response** | riverbero algoritmico (struttura di riflessioni/comb visibile nell'IR!) vs convoluzione; latenza vera |
| Tone burst a varie f | **frequency response** (FR) | il filtro è minimum-phase o linear-phase; gli ordini degli shelf; se l'EQ ha pre-ring |
| Sinusoide a −1 dBFS | **spettro armonico** (THD+N, spurs) | tipo di saturatore (curve pari/dispari), dove rompe, il dither |
| Gradino di soglia sul compressor | **curva di attacco/rilascio + lookahead** | feedforward vs feedback, se il detettore è RMS o peak, forma del ginocchio |
| Impulso a 96 kHz poi downsample | **group delay per frequenza** | la cifra di latenza che *dichiarano* vs quella *vera* (PDC onesto o no) |
| Sweep lento di cutoff | **derivata prima dell'uscita** | come fanno il coefficient ramping: se senti zipper, non lo fanno |
| Silent input, guadagno ×0 | **rumore di fondo/dither** | 32 vs 64 bit interno, noise shaping, se il mix è in f32 |
| Due canali con fase nota | **correlazione/phase response** | come si comporta un tool che pretende di "allineare la fase" |

Perché è un prodotto e non un trucco: **questa harness è esattamente il tuo banco di
accettazione M3/M4.** "Il mio EQ è pulito quanto il riferimento?" smette di essere
un'opinione e diventa una tabella. E hai scritto una volta sola:
`render(signal, plugin) → metrics`.

> **Caveat da rispettare**: (1) alcune EULA di plugin vietano esplicitamente *benchmark e
> pubblicazione* di misure — leggi, e usa le misure per calibrare *il tuo* lavoro, non per
> pubblicare "X ha 34 dB di stopband"; (2) non estrarre/ridistribuire preset o asset;
> (3) con le **trial** alcuni vendor disabilitano funzioni: una misura fatta su una trial
> può mentirti.

### B. Studiare **sorgenti aperti** di DAW e motori

Qui hai il 100% del beneficio del RE con zero rischio: leggi architature *decise*, con i
commenti, e in GPL (che ti autorizza a leggere; ti autorizza solo *meno* a copiare in un
progetto non-GPL — quindi leggi per capire, non per incollare).

| Progetto | Lingua | Cosa vale la pena studiarci |
|---|---|---|
| **Ardour** | C++ (GPL) | **il modello di dati più completo che esista**: `Region`/`Route`/`Patchbay`/`tempo_map`/`undo`/transaction. Se vuoi capire cosa *serve* davvero in un host, è qui. Il suo `tempo_map` è la lezione sul "valutare non accumulare" (§3 di architecture.md). |
| **Mixxx** | C++ (GPL) | catena di effects per-track in realtime, `SoundSourceProvider`, buffer e thread model, beatgrid (transient detection *usata*). Non è una DAW, ma il suo engine di latency/buffering è maturo. |
| **VCV Rack** | C++ (GPL) | **topologia realtime pura**: graph a 1-sample o N-sample, `Buffer`, macro SIMD (`simdAdd` ecc.), dirty-propagation. È il closest thing a un "engine DSP + graph" leggibile. |
| **Petrovich** | **Rust** (GPL) | DAW/host CLAP in Rust, piccolo: è il tuo prior art più diretto per L4 in Rust, e puoi leggerlo tutto in un weekend. |
| **KlangFalter** | **Rust** | convolver plugin reale in Rust: mostra come si fa un processore serio (partitioned convolution, FFT, multi-threading, UI) *in Rust*, non in teoria. |
| **Surge XT** | C++ (GPL) | **il synth open più vicino a Serum**: wavetable, FM, filter multimodo, matrix di modulazione, display via SIMD. Quando arrivi a M8, questa è la mappa. |
| **Camomac plugins** | C++ (GPL) | EQ/compressor/dynamics con **paper di design allegati**: è raro trovare DSP scritto bene *e* spiegato. Ottimo per M2/M4. |
| **Obxd** | C++ (GPL/MIT-ish) | un synth analogico emulato con precisione: lezione di *numerica* applicata (aliasing, non-linearities). |
| **JUCE** (`AudioProcessorGraph`, `VST3Host`) | C++ (GPL/commercial) | come *non* fare un engine (è generalista), ma il suo handling di latenza/`processAudioBlock` e il wrapper VST3 sono una specifica de-facto. |
| **DPF (DISTRHO Plugin Framework)** | C++ (MIT) | plumbing plugin-side (buffer, state, UI) con licenza permissiva: l'unica qui dentro che puoi copiare *legalmente* senza contagio. |
| **Reaper** + sua API | chiuso, **ma** documentatissimo | la documentazione API/JS/SWS di Cockos è di fatto il testo sui "cosa serve a un host". E il loro **`reaperplug-ins` sorgente** (ReaEQ, ReaComp, ReaVerb, ReaFir, ReaMix) è pubblicato da Cockos come sorgente leggibile: DSP *in produzione*, non esercitazioni. È probabilmente la lettura più a buon mercato esistente su "come si scrive un compressore che non fa schifo". (Verifica i termini di quella licenza prima di riusare qualcosa: è "leggi e impara", non necessariamente "ridistribuisci".) |

### C. Specifiche e letteratura: il DSP "vero" non è nei binari

Nessuno dei nostri target (Kick/Bass align, SPAN, FDN, true-peak) richiede RE, perché le
algoritmi sono pubblicati:

* **Will Pirkle**, *An Algorithmic Library for DSP* + *Introduction to Audio Digital
  Effects* — la traduzione più diretta di "voglio scrivere questi plugin": EQ, fase,
  dinamica, riverbero, con le equazioni che si implementano.
* **Robert Bristow-Johnson**, *Audio EQ Cookbook* — perché i nostri coefficienti sono
  quelli, e perché la normalizzazione per `a0` non è opzionale.
* **Rumen Boïatcherov / Zamolodchikov**, gli articoli su EQ e IIR (serie "Audio EQ
  Amplifiers" e note tecniche): la fase e la "Q" in pratica.
* **DAFx book** (Holzapfel & Leeb, PDF libero) — overview classico.
* **BS.1770-4** (ITU-R) per LUFS/true-peak: la norma, con i coefficienti dei filtri di
  pre-media specificati. Non è "un'opinione", è un numero che devi eguagliare.
* **CCRMA / Stanford (Julius Smith, *Spectral Audio Signal Processing*)** — la teoria che
  rende ovvie le decisioni (aliasing, windowing, phase).
* **VST3 SDK**: contiene il **Programming Reference** e degli **host di esempio**. Per il
  ponte VST3 non serve *nessun* RE: è tutto specificato, incluso ciò che un host deve
  fare per essere accettato dai plugin.
* **CLAP** docs + `clap-ossxt`/`clap-host`: l'ABI moderna, leggibile in un giorno.

---

## 3. Il "RE" che ti consiglio, in pratica (2 giorni, non 2 mesi)

Una sessione di **black-box analysis** su plugin che possiedi, con un harness che poi
resta:

```
tools/measure/
  sweep.rs      ← genera swept-sine / MLS / tone burst / step
  render.rs     ← carica un plugin (CLAP prima, VST3 poi) e renderizza offline
  metrics.rs    ← FR, IR, THD+N, group delay, LUFS, max |dy| (zipper), rumore
  report.md     ← tabella: un plugin per riga, i numeri che contano per noi
```

E il punto di arrivo è una tabella che risponde alle domande *nostre*:
- a che frequenza i vari EQ passano da min-phase a lin-phase, e con quale latenza?
- quanto *davvero* vale un "phase align" dichiarato? (misuralo in null test)
- i saturatori che senti "morbidi" che oversampling fanno, e a quale costo di CPU?
- i riverberi algoritmici dei tuoi riferimenti: quante linee di ritardo, che allpass?

Quel `render.rs` è **lo stesso codice** che userai come accettazione per M3/M4. Non stai
facendo RE: stai costruendo il metro con cui misurerai te stesso.

---

## 4. Regola operativa per non farti male

1. **Mai** decompilare un prodotto la cui EULA lo vieta, e mai mentre si progetta il
   equivalente funzionale.
2. Leggi GPL per *capire l'architettura*; se vuoi riusare codice, prendilo da progetti
   **MIT/Apache** (DPF, cpal, rustfft, symphonia, CLAP).
3. Le misure black-box restano **private** come target di progettazione; pubblicare solo
   in forma "il mio X fa Y", mai "il loro X fa Y".
4. UI: ispirati ai *concetti* di workflow (non-modalità, densità, scorciatoie), non alla
   resa. Se un domani somiglia a un altro, il problema è tuo, non del motore.
5. Prima di RE-qualcosa, chiediti: "esiste il paper o la spec?" Nel nostro progetto la
   risposta è **sì in quasi tutti i casi**, e questo è un lusso che l'RE si mangia via.
