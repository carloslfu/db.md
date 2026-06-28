---
type: concept
meta-type: conclusion
id: self-play
created: 2026-05-20T15:00:00Z
updated: 2026-05-23T16:45:00Z
summary: "Training regime where an RL agent generates its own data by playing copies of itself — no expert games, no hand-coded heuristics; powers TD-Gammon and AlphaZero"
topic: self-play
tags: [reinforcement-learning, self-play, training]
derived_from:
  - [[records/papers/silver-alphazero-2017]]
  - [[records/papers/silver-alphago-2016]]
  - [[records/papers/tesauro-tdgammon-1995]]
---

# Self-play

Self-play is a training regime where a reinforcement-learning agent
generates its own training data by playing against copies of itself,
without external expert games or hand-coded heuristics.

## Mechanism

The agent plays against a (possibly older, possibly current) version
of itself. Game outcomes label the trajectories. Policy and value
networks are updated against these labels. Over training, the agent
moves through a curriculum of increasingly competent opponents — itself.

## History

Tesauro's TD-Gammon ([[records/papers/tesauro-tdgammon-1995]]) was the
first famous success: a neural-network backgammon player trained by
self-play that reached world-class strength.

AlphaGo ([[records/papers/silver-alphago-2016]]) used self-play to
refine its policy network by [[records/concepts/policy-gradient]] after a
supervised bootstrap; AlphaZero
([[records/papers/silver-alphazero-2017]]) then dropped the human data
entirely and generalized the recipe: MCTS + neural networks + pure
self-play, master-level chess, shogi, and Go.

## Open questions

- *Stability vs diversity:* pure self-play can converge on a single
  exploit and stop improving — a [[records/concepts/exploration-exploitation]]
  failure at the level of strategies. Population-based methods (League of
  StarCraft II agents) trade compute for diversity.
- *Transfer:* whether self-play in one game transfers to related
  games is unresolved.

## Related concepts

- [[records/concepts/mcts]]
- [[records/concepts/policy-iteration]]
- [[records/concepts/value-network]]
- [[records/concepts/temporal-difference-learning]]
- [[records/concepts/policy-gradient]]
- [[records/concepts/exploration-exploitation]]
