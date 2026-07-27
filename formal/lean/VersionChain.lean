/-
  VersionChain.lean — proof lane `lean-version-chain`, FG-INV-03.core.

  EXACT STATEMENT BEING PROVED (invariants.toml, FG-INV-03.core):

    "Every physical version chain is finite, acyclic, strictly newer-first by
     commit order, and every link remains within one owning object generation."

  Core Lean 4 only: no Mathlib, no `sorry`, no `admit`, no `axiom`, no
  `native_decide`.

  THE MODEL. A physical version chain is presented by its older-link function on
  commit-order positions: `older a = some b` means the link stored at commit
  position `a` points to the next-older version at position `b`. Two structural
  facts are hypotheses, because they are what the storage layer must establish:

    * `newer_first`    — an older link is strictly earlier in commit order;
    * `one_generation` — a link never crosses an owning object generation.

  Everything else is derived. In particular FINITENESS AND ACYCLICITY ARE NOT
  ASSUMED: they are consequences of `newer_first`, which is the mathematical
  content worth checking. A model that assumed them (say, by representing a
  chain as a `List`) would prove nothing about the storage layer.

  WHAT IS NOT COVERED, per the lane's model_scope: arena decode and the fuzz
  oracles. This file is about chain SHAPE, not about the physical representation
  of a link.
-/

namespace FGDB.VersionChain

/-- A physical version chain over commit-order positions. -/
structure Chain where
  /-- The next-older link, if this is not the tail of the chain. -/
  older : Nat → Option Nat
  /-- The owning object generation of the link at a commit position. -/
  gen : Nat → Nat
  /-- Strictly newer-first: an older link is strictly earlier in commit order. -/
  newer_first : ∀ {a b : Nat}, older a = some b → b < a
  /-- Every link remains within one owning object generation. -/
  one_generation : ∀ {a b : Nat}, older a = some b → gen a = gen b

variable {c : Chain}

/-- `Reach c a b` : position `b` is reachable from `a` by following older links. -/
inductive Reach (c : Chain) : Nat → Nat → Prop where
  | refl {a : Nat} : Reach c a a
  | step {a b d : Nat} : c.older a = some b → Reach c b d → Reach c a d

/-- STRICTLY NEWER-FIRST, transitively: everything reachable is no newer. -/
theorem reach_le : ∀ {a b : Nat}, Reach c a b → b ≤ a := by
  intro a b h
  induction h with
  | refl => exact Nat.le_refl _
  | step hab _ ih => exact Nat.le_trans ih (Nat.le_of_lt (c.newer_first hab))

/-- A proper step strictly decreases commit order. -/
theorem reach_lt {a b d : Nat} (hab : c.older a = some b) (hbd : Reach c b d) :
    d < a :=
  Nat.lt_of_le_of_lt (reach_le hbd) (c.newer_first hab)

/-- ACYCLIC: no link can reach its own position again. -/
theorem acyclic {a b : Nat} (hab : c.older a = some b) (hba : Reach c b a) : False :=
  Nat.lt_irrefl a (reach_lt hab hba)

/-- ACYCLIC, stated the other way: reachability is antisymmetric, so the only
    cycle is the empty one. -/
theorem reach_antisymm {a b : Nat} (hab : Reach c a b) (hba : Reach c b a) : a = b :=
  Nat.le_antisymm (reach_le hba) (reach_le hab)

/-- ONE OWNING OBJECT GENERATION, transitively. -/
theorem reach_gen : ∀ {a b : Nat}, Reach c a b → c.gen a = c.gen b := by
  intro a b h
  induction h with
  | refl => rfl
  | step hab _ ih => exact Eq.trans (c.one_generation hab) ih

/-- The chain walked from a position, newest first. Lean accepts this recursion
    only because `newer_first` makes it decrease, which is FINITENESS: the walk
    cannot go on forever. -/
def walk (c : Chain) (a : Nat) : List Nat :=
  match h : c.older a with
  | none => [a]
  | some b => a :: walk c b
termination_by a
decreasing_by exact c.newer_first h

/-- FINITE, with an explicit bound: a chain from position `a` has at most
    `a + 1` links, since every step strictly decreases a natural number. -/
theorem walk_length_le : ∀ (a : Nat), (walk c a).length ≤ a + 1 := by
  intro a
  induction a using Nat.strongInductionOn with
  | _ a ih =>
    rw [walk]
    split
    · simp
      omega
    · next b hb =>
      have hlt : b < a := c.newer_first hb
      have := ih b hlt
      simp only [List.length_cons]
      exact Nat.succ_le_succ (Nat.le_trans this (Nat.succ_le_of_lt hlt))

/-- The walk is never empty: every position is its own newest link. -/
theorem walk_ne_nil (a : Nat) : walk c a ≠ [] := by
  rw [walk]
  split <;> simp

end FGDB.VersionChain
