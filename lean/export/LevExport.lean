import Lean

/-!
# lev-export

`lev-export --out DIR [--only FILE] [--jobs N] ROOT...` imports the roots once and writes
`DIR/manifest.json` and one `DIR/<module>.ndjson` per module (format `lev-export/1`): the
LeanExport 3.1.0 records of the module's own constants, numbered per file from LeanExport's
initial state, and the names of the constants whose bodies importing modules do not see.

The writer is LeanExport's, except that `dumpConstant` writes only the module's own constants:
everything else is referenced through `const` expressions by name, and literals pull in `Nat`,
`Char.ofNat` and `String.ofList` only when the module owns them. Metadata is always stripped and
unsafe and partial constants are never written.
-/

-- The writer is forked from LeanExport/Basic.lean of the Lean toolchain (Copyright (c) 2025 Lean FRO, LLC, Apache-2.0).
open Lean
open Std (HashMap)

def Lean.BinderInfo.toJson : BinderInfo → Json
  | .default => "default"
  | .implicit => "implicit"
  | .strictImplicit => "strictImplicit"
  | .instImplicit => "instImplicit"

def Lean.ReducibilityHints.toJson : ReducibilityHints → Json
  | .opaque => "opaque"
  | .abbrev => "abbrev"
  | .regular n => .mkObj [("regular", n.toNat)]

def Lean.QuotKind.toJson : QuotKind → Json
  | type => "type"
  | ctor => "ctor"
  | lift => "lift"
  | ind => "ind"

def Lean.DefinitionSafety.toJson : DefinitionSafety → Json
  | «unsafe» => "unsafe"
  | safe => "safe"
  | «partial» => "partial"

namespace LevExport

structure Context where
  /-- The module's own constants, without unsafe and partial ones. -/
  own : HashMap Name ConstantInfo
  /-- Maps each inductive type of the module to its recursors. -/
  recursorMap : NameMap NameSet
  out : IO.FS.Handle

structure State where
  visitedNames : HashMap Name Nat := HashMap.emptyWithCapacity 1024 |>.insert .anonymous 0
  visitedLevels : HashMap Level Nat := HashMap.emptyWithCapacity 64 |>.insert .zero 0
  visitedExprs : HashMap Expr Nat := HashMap.emptyWithCapacity 4096
  visitedConstants : NameHashSet := {}
  noMDataExprs : HashMap Expr Expr := {}

abbrev M := ReaderT Context <| StateT State IO

def getOwn (c : Name) : M ConstantInfo := do
  let some info := (← read).own[c]? | throw <| .userError s!"{c} is not a constant of this module"
  return info

/--
For a given primitive (name, level, expr) to be exported:
IFF it's been seen before, return its index within the export file
IFF it has not been seen before, add it to the cache, print it into the export, and return its cache index.
-/
@[inline]
def getIdx [Hashable α] [BEq α] (x : α) (namespaced: String) (getM : State → HashMap α Nat) (setM : State → HashMap α Nat → State) (rec : M Json) : M Nat := do
  let m ← getM <$> get
  if let some idx := m[x]? then
    return idx
  let s ← rec
  let m ← getM <$> get
  let idx := m.size
  (← read).out.putStrLn (s.setObjVal! namespaced idx).compress
  modify fun st => setM st ((getM st).insert x idx)
  return idx

def dumpName (n : Name) : M Nat := getIdx n "in" (·.visitedNames) ({ · with visitedNames := · }) do
  match n with
  | .anonymous => unreachable!
  | .str n s =>
    return .mkObj [
      ("str", .mkObj [
        ("pre", ← dumpName n),
        ("str", s)
      ])
    ]
  | .num n i =>
    return .mkObj [
      ("num", .mkObj [
        ("pre", ← dumpName n),
        ("i", i)
      ])
    ]

def dumpLevel (l : Level) : M Nat := getIdx l "il" (·.visitedLevels) ({ · with visitedLevels := · }) do
  match l with
  | .zero | .mvar _ => unreachable!
  | .succ l => return .mkObj [("succ", ← dumpLevel l)]
  | .max l1 l2 => return .mkObj [("max", Json.arr #[← dumpLevel l1, ← dumpLevel l2])]
  | .imax l1 l2 => return .mkObj [("imax", Json.arr #[← dumpLevel l1, ← dumpLevel l2])]
  | .param n => return .mkObj [("param", ← dumpName n)]

def dumpUparams (uparams : List Name) : M Json := do
  let nameIdxs ← uparams.mapM dumpName
  let _ ← (uparams.map (Level.param)).mapM dumpLevel
  pure nameIdxs.toJson

def dumpNames (uparams : List Name) : M Json := do
  let nameIdxs ← uparams.mapM dumpName
  return nameIdxs.toJson

def removeMData (e : Expr) : M Expr := do
  if let some x := (← get).noMDataExprs[e]? then
    return x
  let e' ← match e with
  | .mdata _ e' => removeMData e'
  | .fvar .. | .mvar ..=> unreachable!
  | .bvar .. | .sort .. | .const .. | .lit _ => pure e
  | .app f a =>
    pure <| e.updateApp! (← removeMData f) (← removeMData a)
  | .lam _ d b _ =>
    pure <| e.updateLambdaE! (← removeMData d) (← removeMData b)
  | .letE _ d v b _ =>
    -- Normalize `nonDep` to `false` to avoid duplicate expression indices.
    -- Lean's `BEq` for `Expr` considers the `nonDep` flag, but external type checkers
    -- (e.g., nanoda_lib) treat expressions as identical regardless of this optimization hint.
    -- Without normalization, the same expression can get different indices, causing parse errors.
    -- See: https://github.com/ammkrn/lean4export/commit/eb023e5
    pure <| e.updateLet! (← removeMData d) (← removeMData v) (← removeMData b) false
  | .forallE _ d b _ =>
    pure <| e.updateForallE! (← removeMData d) (← removeMData b)
  | .proj _ _ e2 =>
    pure <| e.updateProj! (← removeMData e2)
  modify (fun st => { st with noMDataExprs := st.noMDataExprs.insert e e' })
  pure e'

mutual

partial def dumpExprAux (e : Expr) : M Nat := do
  getIdx e "ie" (·.visitedExprs) ({ · with visitedExprs := · }) do
    match e with
    | .fvar .. | .mvar .. | .mdata .. =>
      throw <| .userError "cannot export free variables, metavariables or metadata"
    | .bvar i => return .mkObj [("bvar", i)]
    | .lit (.natVal i) => dumpConstant ``Nat; return .mkObj [("natVal", s!"{i}")]
    | .lit (.strVal s) =>
      dumpConstant ``Char.ofNat
      dumpConstant ``String.ofList
      return .mkObj [("strVal", s)]
    | .sort l => return .mkObj [("sort", ← dumpLevel l)]
    | .const n us => return .mkObj [
      ("const", .mkObj [
        ("name", ← dumpName n),
        ("us", (← us.mapM dumpLevel).toJson)
      ])
    ]
    | .app f a => return .mkObj [
      ("app", .mkObj [
        ("fn", ← dumpExprAux f),
        ("arg", ← dumpExprAux a)
      ])
    ]
    | .lam n d b bi => return .mkObj [
      ("lam", .mkObj [
        ("name", ← dumpName n),
        ("type", ← dumpExprAux d),
        ("body", ← dumpExprAux b),
        ("binderInfo", bi.toJson)
      ])
    ]
    | .forallE n d b bi => return .mkObj [
      ("forallE", .mkObj [
        ("name", ← dumpName n),
        ("type", ← dumpExprAux d),
        ("body", ← dumpExprAux b),
        ("binderInfo", bi.toJson)
      ])
    ]
    | .letE n d v b nondep => return .mkObj [
      ("letE", .mkObj [
        ("name", ← dumpName n),
        ("type", ← dumpExprAux d),
        ("value", ← dumpExprAux v),
        ("body", ← dumpExprAux b),
        ("nondep", nondep)
      ])
    ]
    | .proj s i e => return .mkObj [
      ("proj", .mkObj [
        ("typeName", ← dumpName s),
        ("idx", i),
        ("struct", ← dumpExprAux e)
      ])
    ]

partial def dumpExpr (e : Expr) : M Nat := do
  modify (fun st => { st with noMDataExprs := HashMap.emptyWithCapacity })
  dumpExprAux (← removeMData e)

/-- Writes `c` after the module's constants it depends on, if `c` is one of the module's own constants. -/
partial def dumpConstant (c : Name) : M Unit := do
  let some declar := (← read).own[c]? | return
  if (← get).visitedConstants.contains c then
    return
  modify fun st => { st with visitedConstants := st.visitedConstants.insert c }
  match declar with
  | .axiomInfo val => do
    dumpDeps val.type
    dumpObj [
      ("axiom", Json.mkObj [
        ("name", ← dumpName val.name),
        ("levelParams", ← dumpUparams val.levelParams),
        ("type", ← dumpExpr val.type),
        ("isUnsafe", val.isUnsafe)
      ])
    ]
  | .defnInfo val => do
    dumpDeps val.type
    dumpDeps val.value
    dumpObj [
      ("def", Json.mkObj [
        ("name", ← dumpName val.name),
        ("levelParams", ← dumpUparams val.levelParams),
        ("type", ← dumpExpr val.type),
        ("value", ← dumpExpr val.value),
        ("hints", val.hints.toJson),
        ("safety", val.safety.toJson),
        ("all", ← dumpNames val.all)
      ])
    ]
  | .opaqueInfo val => do
    dumpDeps val.type
    dumpDeps val.value
    dumpObj [
      ("opaque", Json.mkObj [
        ("name", ← dumpName val.name),
        ("levelParams", ← dumpUparams val.levelParams),
        ("type", ← dumpExpr val.type),
        ("value", ← dumpExpr val.value),
        ("all", ← dumpNames val.all),
        ("isUnsafe", val.isUnsafe)
      ])
    ]
  | .thmInfo val => do
    dumpDeps val.type
    dumpDeps val.value
    dumpObj [
      ("thm", Json.mkObj [
        ("name", ← dumpName val.name),
        ("levelParams", ← dumpUparams val.levelParams),
        ("type", ← dumpExpr val.type),
        ("value", ← dumpExpr val.value),
        ("all", ← dumpNames val.all)
      ])
    ]
  | .quotInfo _ =>
    -- Always dump full Quot package in the sensible order (`Eq` only if this module owns it)
    dumpConstant ``Eq
    for c in [`Quot, ``Quot.mk, ``Quot.lift, ``Quot.ind] do
      let .quotInfo val ← getOwn c | throw <| .userError s!"{c} is not a quotient constant"
      modify fun st => { st with visitedConstants := st.visitedConstants.insert c }
      dumpObj [
        ("quot", .mkObj [
          ("name", ← dumpName val.name),
          ("levelParams", ← dumpUparams val.levelParams),
          ("type", ← dumpExpr val.type),
          ("kind", val.kind.toJson)
        ])
      ]
  | .inductInfo baseIndVal => do
    let mut indVals := #[]
    let mut ctorVals := #[]
    let mut recursorNames := NameSet.empty
    for indName in baseIndVal.all do
      let .inductInfo val ← getOwn indName | throw <| .userError s!"{indName} is not an inductive type"
      indVals := indVals.push val
      for ctor in val.ctors do
        let .ctorInfo ctorVal ← getOwn ctor | throw <| .userError s!"{ctor} is not a constructor"
        ctorVals := ctorVals.push ctorVal
      modify fun st => { st with visitedConstants:= st.visitedConstants.insert indName }
      dumpDeps val.type
      if let .some names := (← read).recursorMap.get? baseIndVal.name
      then recursorNames := recursorNames.union names

    /- We dump the constructor dependencies (which will not include the inductives in this block since we've
    added the names to `visitedConstants`) before actually outputting anything in this inductive block to
    ensure e.g. the `LT` in `Fin.mk` is dumped before this inductive block appears in the export file. -/
    for ctorVal in ctorVals do
      modify fun st => { st with visitedConstants:= st.visitedConstants.insert ctorVal.name }
      dumpDeps ctorVal.type

    let mut recursorVals := #[]
    for recursorName in recursorNames do
      let .recInfo recVal ← getOwn recursorName | throw <| .userError s!"{recursorName} is not a recursor"
      recursorVals := recursorVals.push recVal
    for recursorVal in recursorVals do
      modify fun st => { st with visitedConstants:= st.visitedConstants.insert recursorVal.name }
      dumpDeps recursorVal.type

    /- We only dump these after we've already dumped the `recursorVal` type and added the name
    to `visitedConstants` so that `dumpDeps` does not retry dumping the appearance of `Foo.rec` -/
    for recursorVal in recursorVals do
      for rule in recursorVal.rules do
        dumpDeps rule.rhs

    let inductiveValsJson ← indVals.mapM <| fun indVal => do
      pure <| Json.mkObj [
          ("name", ← dumpName indVal.name),
          ("levelParams", ← dumpUparams indVal.levelParams),
          ("type", ← dumpExpr indVal.type),
          ("numParams", indVal.numParams),
          ("numIndices", indVal.numIndices),
          ("all", ← dumpNames indVal.all),
          ("ctors", ← dumpNames indVal.ctors),
          ("numNested", indVal.numNested),
          ("isRec", indVal.isRec),
          ("isReflexive", indVal.isReflexive),
          ("isUnsafe", indVal.isUnsafe),
      ]
    let ctorValsJson ← ctorVals.mapM <| fun ctorVal => do
      pure <| Json.mkObj [
          ("name", ← dumpName ctorVal.name),
          ("levelParams", ← dumpUparams ctorVal.levelParams),
          ("type", ← dumpExpr ctorVal.type),
          ("induct", ← dumpName ctorVal.induct),
          ("cidx", ctorVal.cidx),
          ("numParams", ctorVal.numParams),
          ("numFields", ctorVal.numFields),
          ("isUnsafe", ctorVal.isUnsafe)
      ]
    let recursorValsJson ← recursorVals.mapM <| fun recursorVal => do
      pure <| Json.mkObj [
          ("name", ← dumpName recursorVal.name),
          ("levelParams", ← dumpUparams recursorVal.levelParams),
          ("type", ← dumpExpr recursorVal.type),
          ("all", ← dumpNames recursorVal.all),
          ("numParams", recursorVal.numParams),
          ("numIndices", recursorVal.numIndices),
          ("numMotives", recursorVal.numMotives),
          ("numMinors", recursorVal.numMinors),
          ("rules", (← recursorVal.rules.mapM dumpRecRule).toJson),
          ("k", recursorVal.k),
          ("isUnsafe", recursorVal.isUnsafe),
      ]
    dumpObj [
      ("inductive", Json.mkObj [
        ("types", inductiveValsJson.toJson),
        ("ctors", ctorValsJson.toJson),
        ("recs", recursorValsJson.toJson),
      ])
    ]
  | .ctorInfo val => dumpConstant val.induct
  | .recInfo val =>
    for indName in val.all do
      dumpConstant indName
where
  dumpDeps e := do
    for c in e.getUsedConstants do
      dumpConstant c
  /- Return these for inclusion inline with the exported `recInfo`. -/
  dumpRecRule (rule : RecursorRule) : M Json := do
    return Json.mkObj [
      ("ctor", ← dumpName rule.ctor),
      ("nfields", rule.nfields),
      ("rhs", ← dumpExpr rule.rhs),
    ]
  dumpObj (fields : List (String × Json)) : M Unit := do
    (← read).out.putStrLn <| Json.mkObj fields |>.compress

end

def exportMetadata : Json :=
  let leanMeta := Json.mkObj [
    ("version", versionString),
    ("githash", githash)
  ]
  let exporterMeta := Json.mkObj [
    ("name", "lean4export"),
    ("version", "3.1.0")
  ]
  let formatMeta := Json.mkObj [
    ("version", "3.1.0")
  ]

  Json.mkObj [
    ("meta", Json.mkObj [
      ("exporter", exporterMeta),
      ("lean", leanMeta),
      ("format", formatMeta)
    ])
  ]

/--
Writes the export file of module `mod` with own constants `consts`: the metadata line, the
records of `consts` (each after the constants of `consts` it references), and a last line with
the name indices of the constants for which `hidden` holds.
-/
def dumpModule (mod : Name) (consts : Array ConstantInfo) (hidden : ConstantInfo → Bool)
    (out : IO.FS.Handle) : IO Unit := do
  let own := consts.foldl (init := HashMap.emptyWithCapacity consts.size) fun m c => m.insert c.name c
  let recursorMap := consts.foldl (init := ({} : NameMap NameSet)) fun m c =>
    match c with
    | .recInfo r => r.all.foldl (init := m) fun m ind =>
      m.alter ind fun recs? => some ((recs?.getD {}).insert r.name)
    | _ => m
  let lev := Json.mkObj [("exporter", "lev-export/1"), ("module", toString mod)]
  out.putStrLn <| "{\"meta\":" ++ (exportMetadata.getObjValD "meta").compress ++
    ",\"lev\":" ++ lev.compress ++ "}"
  let act : M Unit := do
    for c in consts do
      modify fun st => { st with noMDataExprs := {} }
      dumpConstant c.name
    let st ← get
    let mut idxs : Array Nat := #[]
    for c in consts do
      unless st.visitedConstants.contains c.name do
        throw <| .userError s!"{c.name} was not written"
      if hidden c then
        idxs := idxs.push st.visitedNames[c.name]!
    out.putStrLn (Json.mkObj [("lev", Json.mkObj [("hidden", toJson idxs)])]).compress
  act.run { own, recursorMap, out } |>.run' {}

open System

def usage := "usage: lev-export --out DIR [--only FILE] [--jobs N] ROOT..."

structure Args where
  out : Option FilePath := none
  only : Option FilePath := none
  jobs : Nat := 1
  roots : Array Name := #[]

def parseName (s : String) : Except String Name :=
  match Syntax.decodeNameLit ("`" ++ s) with
  | some n => .ok n
  | none => .error s!"invalid module name '{s}'"

def parseArgs : List String → Args → Except String Args
  | "--out" :: dir :: rest, a => parseArgs rest { a with out := some dir }
  | "--only" :: file :: rest, a => parseArgs rest { a with only := some file }
  | "--jobs" :: n :: rest, a => match n.toNat? with
    | some k@(_ + 1) => parseArgs rest { a with jobs := k }
    | _ => .error s!"invalid job count '{n}'"
  | arg :: rest, a =>
    if arg.startsWith "-" then .error s!"unknown option '{arg}'"
    else do parseArgs rest { a with roots := a.roots.push (← parseName arg) }
  | [], a => .ok a

/--
`importModules` at the private level, so theorem bodies are present, but with every module's
exported data also loaded into the public constant map. `env.setExporting true` then shows each
constant as a module that imports it without `import all` sees it.
-/
def importEnv (roots : Array Name) : IO Environment := withImporting do
  let imports : Array Import := roots.map ({ module := · })
  let (_, s) ← (importModulesCore imports).run
  finalizeImport s.markAllExported imports {} (leakEnv := true) (loadExts := false) (isModule := true)

/--
Whether importing modules see `c` without its body: theorems of module files, and definitions and
opaques that a module file exports as axioms or not at all. `pub` is the exported view.
-/
def isHidden (pub : Environment) (isModule : Bool) (c : ConstantInfo) : Bool :=
  isModule && match c with
    | .thmInfo _ => true
    | .defnInfo _ | .opaqueInfo _ => !(pub.find? c.name |>.any (·.hasValue (allowOpaque := true)))
    | _ => false

def fileName (mod : Name) : String :=
  mod.toString (escape := false) ++ ".ndjson"

/-- The `.olean`, `.olean.private`, `.olean.server` and `.ir` files of `mod` that exist. -/
def artifacts (mod : Name) : IO (Array FilePath) := do
  let mut olean ← findOLean mod
  unless olean.isAbsolute do olean := (← IO.currentDir) / olean
  let files := #[olean, OLeanLevel.adjustFileName olean .private,
    OLeanLevel.adjustFileName olean .server, olean.withExtension "ir"]
  files.filterM fun p => do return (← p.pathExists)

def writeAtomically (path : FilePath) (write : IO.FS.Handle → IO Unit) : IO Unit := do
  let tmp := path.addExtension "tmp"
  let h ← IO.FS.Handle.mk tmp .write
  write h
  h.flush
  IO.FS.rename tmp path

def exportModule (env pub : Environment) (dir : FilePath) (i : Nat) : IO Unit := do
  let mod := env.header.moduleNames[i]!
  let data := env.header.moduleData[i]!
  let consts := data.constants.filter fun c => !c.isUnsafe && !c.isPartial
  writeAtomically (dir / fileName mod) (dumpModule mod consts (isHidden pub data.isModule))

def manifest (env : Environment) (roots : Array Name) (arts : Array (Array FilePath)) : Json :=
  let modules := env.header.moduleNames.mapIdx fun i mod =>
    let data := env.header.moduleData[i]!
    -- `meta` and `public` are not recorded, so e.g. the implicit `import Init` and
    -- `meta import Init` of a module file become one entry.
    let imports := data.imports.foldl (init := #[]) fun acc imp =>
      if acc.contains (imp.module, imp.importAll) then acc else acc.push (imp.module, imp.importAll)
    Json.mkObj [
      ("name", toString mod),
      ("file", fileName mod),
      ("imports", toJson <| imports.map fun (m, all) =>
        Json.mkObj [("module", toString m), ("all", all)]),
      ("isModule", data.isModule),
      ("artifacts", toJson <| arts[i]!.map (·.toString))]
  Json.mkObj [
    ("format", "lev-export/1"),
    ("lean", Json.mkObj [("version", versionString), ("githash", githash)]),
    ("roots", toJson <| roots.map toString),
    ("modules", toJson modules)]

def run (a : Args) (dir : FilePath) : IO UInt32 := do
  let t0 ← IO.monoMsNow
  let env ← importEnv a.roots
  let pub := env.setExporting true
  let mods := env.header.moduleNames
  let t1 ← IO.monoMsNow
  IO.eprintln s!"lev-export: imported {mods.size} modules in {t1 - t0} ms"
  let mut todo := Array.range mods.size
  if let some file := a.only then
    let mut idxOf : Std.HashMap Name Nat := {}
    for h : i in [0:mods.size] do
      idxOf := idxOf.insert mods[i] i
    todo := #[]
    for line in ← IO.FS.lines file do
      let line := line.trimAscii.copy
      if line.isEmpty then continue
      let some i := idxOf[← IO.ofExcept (parseName line)]?
        | throw <| .userError s!"{line} is not a module of the imported environment"
      unless todo.contains i do todo := todo.push i
  let arts ← mods.mapM artifacts
  -- Biggest modules first, so that the last jobs to finish are small ones.
  let size (i : Nat) : IO Nat :=
    arts[i]!.foldlM (fun n p => do return n + (← p.metadata).byteSize.toNat) 0
  let sizes ← todo.mapM fun i => do return (i, ← size i)
  let order := sizes.qsort (fun a b => a.2 > b.2) |>.map (·.1)
  IO.FS.createDirAll dir
  let next ← IO.mkRef 0
  let errors ← IO.mkRef (#[] : Array String)
  let worker : IO Unit := do
    repeat
      let k ← next.modifyGet fun k => (k, k + 1)
      let some i := order[k]? | break
      try exportModule env pub dir i
      catch e => errors.modify (·.push s!"{mods[i]!}: {e}")
  let tasks ← (List.range a.jobs).mapM fun _ => IO.asTask worker (prio := .dedicated)
  for t in tasks do
    if let .error e ← IO.wait t then errors.modify (·.push (toString e))
  let errors ← errors.get
  for e in errors do IO.eprintln s!"lev-export: {e}"
  let t2 ← IO.monoMsNow
  IO.eprintln s!"lev-export: wrote {todo.size - errors.size} of {todo.size} modules in {t2 - t1} ms with {a.jobs} jobs"
  unless errors.isEmpty do return 1
  writeAtomically (dir / "manifest.json") fun h => h.putStrLn (manifest env a.roots arts).compress
  return 0

end LevExport

open LevExport in
def main (args : List String) : IO UInt32 := do
  let a ← IO.ofExcept <| (parseArgs args {}).mapError (s!"{·}\n{usage}")
  let some dir := a.out | throw <| .userError usage
  if a.roots.isEmpty then throw <| .userError usage
  initSearchPath (← findSysroot)
  run a dir
