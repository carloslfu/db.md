---
type: index
scope: type-folder
folder: wiki/concepts
updated: 2026-05-22T11:30:00Z
---

# wiki/concepts

- [[wiki/concepts/value-network]] — A neural network that estimates the expected outcome from a position; TD-Gammon learned one by temporal-difference self-play, AlphaZero shares a value head with its policy  ·  #reinforcement-learning #function-approximation #value-network
- [[wiki/concepts/policy-iteration]] — Classic dynamic-programming loop alternating policy evaluation and policy improvement; AlphaZero realizes it with MCTS as the improvement operator  ·  #reinforcement-learning #dynamic-programming #policy-iteration
- [[wiki/concepts/mcts]] — Monte Carlo Tree Search: a best-first search that builds an asymmetric tree via repeated simulation; in AlphaZero a neural net replaces the random rollout  ·  #reinforcement-learning #search #planning #mcts
- [[wiki/concepts/self-play]] — Training regime where an RL agent generates its own data by playing copies of itself — no expert games, no hand-coded heuristics; powers TD-Gammon and AlphaZero  ·  #reinforcement-learning #self-play #training
- [[wiki/concepts/markov-decision-process]] — The formal model underneath reinforcement learning: states, actions, transition probabilities, rewards, and a discount factor, with the Markov property  ·  #reinforcement-learning #theory #foundations #markov-decision-process
