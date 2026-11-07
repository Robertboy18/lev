-- Fixture for run.sh: a module-system file with exposed and hidden definitions, theorems, an
-- opaque, a partial and an unsafe definition, mutual and nested inductives, and literals.
module

public section

inductive Color where
  | red | green | blue

structure Point where
  x : Nat
  y : Nat

def Point.add (p q : Point) : Point := ⟨p.x + q.x, p.y + q.y⟩

@[expose] def Point.zero : Point := ⟨0, 0⟩

instance : Inhabited Point := ⟨Point.zero⟩

theorem Point.zero_x : Point.zero.x = 0 := rfl

abbrev Pair := Nat × Nat

mutual
  inductive Tree where
    | node : Forest → Tree
  inductive Forest where
    | nil : Forest
    | cons : Tree → Forest → Forest
end

mutual
  def Tree.size : Tree → Nat
    | .node f => f.size + 1
  def Forest.size : Forest → Nat
    | .nil => 0
    | .cons t f => t.size + f.size
end

inductive Rose where
  | node : List Rose → Rose

@[expose] def greeting : String := "hello, world"

def big : Nat := 123456789012345678901234567890

theorem big_pos : 0 < big := by decide

opaque secret : Nat

partial def loop (n : Nat) : Nat := loop (n + 1)

unsafe def danger : Nat := 7

end

def helper (n : Nat) : Nat := n + 1

public def usesHelper (n : Nat) : Nat := helper n
