-- Fixture for run.sh: a file outside the module system.
import Mod2

inductive Shape where
  | circle (r : Nat)
  | square (s : Nat)

def Shape.area : Shape → Nat
  | .circle r => 3 * r * r
  | .square s => s * s

theorem Shape.area_square (s : Nat) : (Shape.square s).area = s * s := rfl

def msg : String := "plain"

theorem Point.zero_x' : Point.zero.x = 0 := rfl
