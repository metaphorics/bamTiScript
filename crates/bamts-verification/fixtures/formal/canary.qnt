module formal_canary {
  type CanaryAction = Init | Increment({ amount: int }) | Reset

  var state: { value: int, resets: int }
  var transition: CanaryAction

  action init = all {
    state' = { value: 0, resets: 0 },
    transition' = Init,
  }

  action increment = {
    nondet amount = Set(1, 2).oneOf()
    all {
      state.value == 0,
      state' = { value: state.value + amount, resets: state.resets },
      transition' = Increment({ amount: amount }),
    }
  }
  action reset = all {
    state.value > 0,
    state' = { value: 0, resets: state.resets + 1 },
    transition' = Reset,
  }

  action step = any {
    increment,
    reset,
  }
}
