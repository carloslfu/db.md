---
type: concept
meta-type: conclusion
id: function-approximation
created: 2026-04-14T17:00:00Z
updated: 2026-05-23T15:25:00Z
summary: "Representing value functions or policies with a parameterized approximator (e.g. a neural net) to generalize across large state spaces; one leg of the deadly triad"
topic: function-approximation
tags: [reinforcement-learning, function-approximation, deep-rl, foundations]
derived_from:
  - [[records/papers/mnih-dqn-2015]]
  - [[records/papers/tesauro-tdgammon-1995]]
---

# Function approximation

Function approximation is the use of a parameterized function — a linear
model, a tile coding, or most often today a neural network — to represent a
value function or policy, so that the agent can **generalize** across states
instead of storing one entry per state. It is what lets reinforcement
learning escape small, enumerable problems and operate on images, board
positions, and continuous control.

## Why it is necessary

The tabular methods that come with clean convergence guarantees —
[[records/concepts/q-learning]], [[records/concepts/policy-iteration]] —
need one table entry per state or state-action pair. For an Atari frame or
a Go board the state space is astronomically large and almost every state
is seen at most once. A table cannot generalize from a state it has visited
to a similar one it has not. An approximator can: it shares parameters
across states, so experience in one state improves the estimate in nearby
states.

## History

TD-Gammon ([[records/papers/tesauro-tdgammon-1995]]) was the early proof
that a nonlinear approximator — a neural-network
[[records/concepts/value-network]] trained by
[[records/concepts/temporal-difference-learning]] — could reach world-class
play, well before the theory of nonlinear TD was settled. DQN
([[records/papers/mnih-dqn-2015]]) generalized the lesson to learning
directly from pixels across dozens of games with a single architecture, and
is the result that made "deep reinforcement learning" a field.

## The deadly triad

Function approximation is one leg of the **deadly triad**: bootstrapping
(TD), off-policy training, and function approximation are each safe in
isolation but can **diverge** when all three are combined. The danger is
that a generalizing approximator updates the value of states it is not
currently visiting, and an off-policy bootstrapped target can amplify those
errors without bound. Most of the engineering in deep value-based RL is a
response to this:

- **Target networks** freeze the bootstrap target for a fixed interval so
  it cannot chase the online estimate.
- **[[records/concepts/experience-replay]]** breaks the temporal
  correlation of samples and stabilizes the gradient.

On the policy side, [[records/concepts/trust-region-methods]] play an
analogous stabilizing role for approximated policies.

## Open questions

- *Convergence:* there is no general convergence guarantee for nonlinear
  off-policy TD; the deadly triad has no universal cure, only mitigations.
- *Representation:* what an approximator generalizes well over depends on
  its inductive bias; choosing or learning the right representation remains
  central.

## Related concepts

- [[records/concepts/value-network]]
- [[records/concepts/temporal-difference-learning]]
- [[records/concepts/experience-replay]]
- [[records/concepts/q-learning]]
- [[records/concepts/trust-region-methods]]
