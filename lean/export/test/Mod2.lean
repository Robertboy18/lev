-- Fixture for run.sh: a module-system file with `import all`.
module

public import Mod
import all Mod

public theorem Point.add_zero_x (p : Point) : (p.add Point.zero).x = p.x := by
  simp [Point.add, Point.zero]

public def twice (n : Nat) : Nat := usesHelper (usesHelper n)
