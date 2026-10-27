# Rust per questo progetto — dalla prospettiva di chi viene da Java/JS/Python

Niente "cos'è una variabile". Qui c'è solo ciò che serve a *questo* codebase, spiegato
con gli errori **realmente accaduti in questa repo** (cerca "È successo qui"). Se il
borrow checker ti blocca, cerca il concetto qui sotto prima di combattere a caso.

---

## 1. Ownership: la differenza mentale numero uno

In Java/JS un oggetto ha N riferimenti e un GC decide quando muore. In Rust **ogni
valore ha un proprietario singolo**; quando il proprietario esce dallo scope, il valore
muore. Passare un valore di default lo **muove** (il mittente non può più usarlo), non lo
copia.

```rust
let buf = ChannelBuffer::new(2, 64);   // buf POSSIEDE 512 f32 sullo heap
let b = buf.block();                    // b è una VISTA (prestito) su buf: non possiede niente
// fine scope: b muore (zero costi), poi buf libera i 512 f32. Ordine garantito.
```

Perché ci interessa in audio: **"chi libera questa memoria, e quando?" diventa una
domanda con risposta statica**. In un thread realtime non puoi permetterti che un free
capiti nel callback; in Rust, se il callback non possiede nulla, non libera nulla. È la
base strutturale (non disciplinare!) della regola "zero allocazioni nel callback".

`Copy`: i tipi piccoli e banali (`Shape`, `ProcessCtx`, `Coeffs`) si marcano
`#[derive(Clone, Copy)]` e tornano a comportarsi come i numeri di Java/JS: passaggio per
valore, zero sorprese. **Regola pratica**: tutto ciò che attraversa il callback è `Copy`
o è un prestito.

## 2. Borrowing: la regola d'oro

O, in italiano: *o molti lettori, o uno scrittore — mai entrambi*.

```rust
fn process(&mut self, ctx: &ProcessCtx, block: &mut Block<'_>)
//         ^^^^^^^ proprietario mutabile esclusivo dello stato del nodo
//                        ^^^ sola lettura, condivisibile
//                                       ^^^ mutabile: il blocco lo modifica IL nodo
```

Il compilatore *dimostra* che nessun altro thread può leggere `self` mentre `process` lo
muta. In Java scriveresti `synchronized` (o peggio, niente, sperando). Qui l'assenza di
data race è un teorema, non una convenzione. **È per questo che il progetto è in Rust**:
tu sei solo, e questo teorema è il tuo code reviewer.

Eccezione che useremo spesso: l'interno condiviso (§4).

## 3. Lifetime: `'a` demistificato

`'a` non aggiunge tempo di vita a niente: **descrive** per quanto un prestito è valido.

```rust
pub struct Block<'a> { data: &'a mut [f32], shape: Shape }
```

Leggilo: "un `Block` contiene un prestito a dei float; `'a` è l'etichetta di *quanto
vive quel prestito*". Il compilatore usa l'etichetta per provare che il `Block` non
sopravvive al buffer. Tutta la difficoltà dell'arena audio (avere N buffer scambiabili
nel grafo) è un problema di lifetime: i prestiti multipli simultanei a uno stesso
proprietario non sono esprimibili direttamente, e la soluzione standard è
**handle + closure**:

```rust
buffers.with(id_a, |blk| { ... });          // il prestito vive solo dentro la closure
buffers.with2(id_a, id_b, |a, b| { ... });  // due prestiti disgiunti, dimostrati tali
```

**È successo qui**: il primo design di `Scratch` (bump allocator che ritorna `Block<'a>`
da `&mut self`) non compilava — il prestito di ritorno teneva in ostaggio tutto
`self`. Morale: se combatti i lifetime su un'arena, passa a indici + closure. → M1.

## 4. Interior mutability: i 4 strumenti, quando usarli

A volte serve mutare attraverso un riferimento condiviso (`&self`). Rust lo permette con
tipi espliciti — e la scelta dice *cosa stai affermando*:

| Tipo | Costo | Quando | Nel progetto |
|---|---|---|---|
| `Cell<T>` | zero | valori Copy, singolo thread | stato TLS della guardia allocazioni |
| `AtomicU32/U64` | ~zero su x86 | **condivisione fra thread** senza lock | `AtomicParam` (UI→audio) |
| `RefCell<T>` | check a runtime | mutabilità complessa, singolo thread | (da evitare nel path audio: il check può fallire a runtime) |
| `UnsafeCell<T>` | zero, senza reti | stai scrivendo una struttura dati | solo dentro wrapper auditati |

Il bit-cast del float nell'atomico è il pattern da ricordare:

```rust
bits: AtomicU32::new(value.to_bits())   // f32 non è Atomic; u32 sì
f32::from_bits(self.bits.load(Acquire)) // 32 bit: load/store atomiche per hardware
```

`Release`/`Acquire` garantiscono che chi legge veda anche le scritture *precedenti* alla
store. Su x86 un load/store a 32 bit allineato è già atomico: costo zero, nessuna lock.

## 5. `thread_local!` + const-init: il caso di studio della guardia

Il requisito: l'allocator globale deve sapere "sono sul thread audio?" a ogni `alloc`.
Un `static` globale no (è per-processo, non per-thread). Un `Mutex<ThreadId>` no (lock
nell'allocator = disastro). La risposta è TLS — ma con due trappole mortali:

1. Un `thread_local!` con initializer **lazy** alloca/registra al primo accesso:
   ricorsione dentro l'allocator → stack overflow.
2. Un tipo con `Drop` registra un destructor TLS: altra allocazione, altro disastro.

La soluzione nel codice:

```rust
thread_local! {
    static STATE: Cell<u8> = const { Cell::new(S_IDLE) };  // const-init, Cell<u8> non ha Drop
}
```

`const { }` + tipo senza Drop = accesso TLS compilato a lettura diretta, zero
allocazioni, zero destructor. **È successo qui**: è il motivo per cui lo stato è un banale
`u8` e non un enum "ricco" — l'enum andrebbe bene uguale (niche optimization), ma la
regola "solo tipi trivially-Drop dentro il TLS dell'allocator" non si discute.

La terza trappola, risolta: **panic dentro l'allocator**. Il panic alloca (payload,
backtrace); se l'allocator panico su ogni allocazione, il panic stesso alloca →
ricorsione infinita. La guardia passa lo stato a `S_REPORTING` *prima* di panico, e in
quello stato le allocazioni sono lecite. Il meccanismo si auto-limita.

## 6. Trait, `dyn`, e il costo dell'astrazione

```rust
pub trait Node {
    fn process(&mut self, ctx: &ProcessCtx, block: &mut Block<'_>) -> Result<(), NodeError>;
    fn latency_samples(&self) -> usize { 0 }   // default: la maggior parte dei nodi è a latenza 0
}
```

Due modi di usarlo:
- `impl<T: Node>` / generics: **monomorfizzazione**, il compilatore genera codice per
  ogni tipo concreto, chiama diretta, inlineabile. Massimo desempenho.
- `Box<dyn Node>`: **vtable**, chiamata indiretta. Flessibile (un `Vec` di nodi eterogenei
  = il grafo), costo ~1 chiamata indiretta per blocco. Su un blocco da 64 frame,
  trascurabile. Su un inner loop di sample, no.

Regola del progetto: `dyn Node` **per blocco**, mai per sample. Dentro `process` il
codice è concreto e si inlinera.

Object safety: un trait con metodi che ritornano `Self` o generics non è `dyn`-compatibile.
Il `Node` trait è disegnato per esserlo (nessun `Self` di ritorno). Se un giorno ti serve
`fn clone_box(&self) -> Box<dyn Node>`, quello è il pattern (clone attraverso la vtable).

## 7. Errori: `Result` senza allocazioni

```rust
pub enum NodeError {
    BufferTooSmall { have: usize, need: usize },
    InvalidParam(&'static str),      // NOT String: una String qui allocherebbe
    Internal(&'static str),          // esattamente quando il sistema è già in difficoltà
}
```

`Result<T, E>` è l'eccezione di Java con il tipo nell'firma: impossibile ignorarla (a
meno di `let _ =`, esplicito). `?` propaga. Nel path audio gli errori sono **valori
statici**: niente heap. E niente `panic!` se non dove *vogliamo* esplodere (la guardia,
i test).

## 8. `unsafe`: disciplina, non magia

`unsafe` non disattiva i controlli: ti permette 5 operazioni (deref di puntatore grezzo,
chiamare fn `unsafe`, etc.) e **sposta la prova di correttezza su di te**. La disciplina
del progetto:

1. **Blast radius**: `unsafe` confinato in funzioni piccole con precondizioni scritte
   nel `# Safety` della doc-comment. Chi legge sa dove finisce la garanzia.
2. `#![deny(unsafe_op_in_unsafe_fn)]`: dentro una `unsafe fn`, ogni operazione unsafe
   vuole il *suo* blocco `unsafe { }` con il *suo* commento SAFETY. Sembra pedante; è ciò
   che rende auditabile. **È successo qui**: il kernel AVX2 aveva load/store fuori dal
   blocco (compilava in edition 2021 con solo warning, ma il nostro lint lo ha promosso a
   errore) — e rileggere la funzione per fixing è esattamente il momento in cui è emerso
   il bug `−a2·out`. Il lint ha letteralmente trovato il bug di fase.
3. `#[target_feature(enable = "avx2,fma")]` + `is_x86_feature_detected!` a runtime: la
   funzione con intrinsic vive isolata, il dispatch (`process_detected`) verifica la CPU
   prima di chiamarla. Senza: `SIGILL` sulla macchina vecchia. Mai fidarsi che "ormai
   tutte le CPU hanno AVX2".

## 9. Iteratori e zero-cost abstraction

```rust
for ch in block.channels_mut() {          // = data.chunks_exact_mut(frames)
    for (out, inp) in d[..fr].iter_mut().zip(&s[..fr]) { *out += *inp * gain; }
}
```

In release questo compila nello stesso assembly di un doppio `for i in 0..n` scritto a
mano — ma senza i bound-check nel loop e senza poter sbagliare gli indici. Le catene
`iter().map().filter().collect()` sono per il codice di **setup**; nel path caldo preferiamo
i loop espliciti su slice quando la forma è semplice (leggibilità del generated code con
`cargo asm`/Compiler Explorer).

Per i benchmark: `std::hint::black_box(x)` impedisce al compilatore di ottimizzare via
il calcolo che stai misurando. Senza, misuri una funzione vuota.

## 10. Cargo: la build config È codice

```toml
[profile.release]
opt-level = 3
lto = "fat"           # link-time optimization fra crate: inlinea hathor-dsp-filters DENTRO hathor-audio-probe
codegen-units = 1     # niente parallelismo di codegen: ottimizzazioni globali migliori
# panic = "abort"     # VIETATO qui: i test della guardia usano catch_unwind
```

Per un progetto DSP queste tre righe valgono spesso il 20-40% di throughput rispetto alla
build default. `lto = "fat"` in particolare è ciò che rende `Box<dyn Node>` + crate
separati quasi gratis: le chiamate piccole si inlineano attraverso i confini di crate.

Workspace: versioni delle dipendenze nel `Cargo.toml` radice (`[workspace.dependencies]`),
i membri ereditano (`{ workspace = true }`). Un solo posto dove aggiornare `cpal`.

## 11. Test: la specifica eseguibile

Quattro pattern di questo progetto, tutti nati da un bug reale:

1. **Reference indipendente**: `df2t_matches_f64_reference` confronta f32-runtime con
   f64-naive. Ma la reference l'ho scritta io → conteneva lo stesso bug. Lezione: una
   reference scritta dalla stessa testa che ha scritto il kernel non è una prova.
2. **Test comportamentale**: `measured_response_matches_predicted_response` misura il filtro
   *dall'esterno* (sine in → ampiezza/fase out) e confronta con la teoria (`Coeffs::response`).
   Due cammini indipendenti: algebra vs ricorrenza. Questo ha beccato il bug.
3. **Test del meccanismo di test**: `audio_section_resets_state_after_panic` verifica
   che la guardia funzioni davvero (catch_unwind su un panic-in-allocator). Una guardia
   non testata è decorazione.
4. **Fix del lint ≠ fix sicuro**: il suggerimento clippy `vec_init_then_push` → `vec![..]`
   cambia il cammino di allocazione (`Box`/`exchange_malloc`, percorso di fallimento
   **nounwind** in release) e la sera del rilascio il panic della guardia non veniva più
   preso da `catch_unwind` — test rotti **solo in release**, debug verde. L'example
   `guard_probe.rs` è la prova eseguibile (`--release --example guard_probe -- vec|push`).
   Lezione: **debug verde non prova nulla sull'unwinding: la gate è release**.

Più: RNG deterministico nei test (`Lcg`, non `rand::thread_rng`) — un test numerico che
cambia input a ogni run non è riproducibile; e tolleranze giustificate (FMA arrotonda una
volta sola, lo scalar due: il kernel AVX2 è *più preciso*, non "diverso a caso", e il
test lo dice invece di nasconderlo dietro `1e-3`).

## 12. Mappa rapida dal tuo mondo

| Java/JS/Python | Rust | Nota |
|---|---|---|
| GC | ownership + Drop | deterministico: il free avviene dove lo vedi |
| `final` field | campo senza `mut` | default immutabile, opt-in al mutabile |
| `synchronized`/`volatile` | `Mutex<T>`/`Atomic` | ma prima: `&mut` esclusivo, spesso basta quello |
| interface | trait | + default methods, + `dyn` (vtable) o generics (mono) |
| eccezione | `Result<T,E>` + `?` | negli errori realtime: E senza heap |
| `null` | `Option<T>` | `unwrap()` è il tuo NPE volontario: `deny` fuori dai test |
| arraylist | `Vec` (setup) / slice `&[T]` (runtime) | nel callback: solo slice di roba preallocata |
| JIT warmup | nessuno | la build release È il prodotto; niente sorprese a runtime |
| `ThreadLocal` | `thread_local!` | con const-init se tocchi l'allocator (§5) |

## Esercizi

1. In `buffer.rs`, prova a far restituire a `ChannelBuffer::block()` un `Block<'static>`:
   leggi l'errore del compilatore per intero. È la migliore spiegazione di lifetime che
   riceverai gratis.
2. Scrivi un `Node` finto che alloca un `Vec` dentro `process`, avvolgi il test in
   `AudioSection` e guardalo esplodere. Poi toglie l'allocazione e guarda il contatore a 0.
3. Cambia `panic = "abort"` nel profile release e rilancia i test: leggi *perché* il
   workspace lo vieta nel commento del `Cargo.toml`.
4. Con Compiler Explorer (godbolt.org, rustc 1.9x, `-O --crate-type=lib`): incolla
   `Biquad::process` e trova le istruzioni della ricorrenza. Conta i `mulss`/`addss`:
   vedrai la DF2T letteralmente.
