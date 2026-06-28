---
type: index
scope: type-folder
folder: records/concepts
updated: 2026-05-23T16:45:00Z
---

# records/concepts

- [[records/concepts/mcts]] — Monte Carlo Tree Search: a best-first search that builds an asymmetric tree via repeated simulation; in AlphaZero a neural net replaces the random rollout  ·  #reinforcement-learning #search #planning #mcts
- [[records/concepts/policy-iteration]] — Classic dynamic-programming loop alternating policy evaluation and policy improvement; AlphaZero realizes it with MCTS as the improvement operator  ·  #reinforcement-learning #dynamic-programming #policy-iteration
- [[records/concepts/self-play]] — Training regime where an RL agent generates its own data by playing copies of itself — no expert games, no hand-coded heuristics; powers TD-Gammon and AlphaZero  ·  #reinforcement-learning #self-play #training
- [[records/concepts/value-network]] — A neural network that estimates the expected outcome from a position; TD-Gammon learned one by temporal-difference self-play, AlphaZero shares a value head with its policy  ·  #reinforcement-learning #function-approximation #value-network
- [[records/concepts/trust-region-methods]] — Policy-gradient methods that bound how far each update moves the policy — TRPO via a KL trust region, PPO via a clipped ratio — to keep learning stable  ·  #reinforcement-learning #policy-gradient #trust-region #trpo #ppo
- [[records/concepts/experience-replay]] — Storing past transitions in a buffer and training on random minibatches from it; decorrelates samples and reuses data, a key stabilizer behind DQN  ·  #reinforcement-learning #experience-replay #deep-rl #dqn
- [[records/concepts/function-approximation]] — Representing value functions or policies with a parameterized approximator (e.g. a neural net) to generalize across large state spaces; one leg of the deadly triad  ·  #reinforcement-learning #function-approximation #deep-rl #foundations
- [[records/concepts/exploration-exploitation]] — The dilemma of choosing between exploiting the best-known action and exploring to discover better ones; ε-greedy, UCB, and entropy bonuses are practical answers  ·  #reinforcement-learning #exploration #bandits #foundations
- [[records/concepts/actor-critic]] — Architecture pairing a policy (actor) with a learned value function (critic) that supplies a low-variance advantage signal; A3C scaled it with parallel workers  ·  #reinforcement-learning #actor-critic #policy-gradient #a3c
- [[records/concepts/policy-gradient]] — Optimizing a parameterized policy directly by ascending the gradient of expected return (REINFORCE); the basis for actor-critic, TRPO, and PPO  ·  #reinforcement-learning #policy-gradient #reinforce #foundations
- [[records/concepts/q-learning]] — Off-policy TD control that learns the optimal action-value function Q*(s,a) directly via a max over next-state actions; the algorithm DQN scaled to pixels  ·  #reinforcement-learning #q-learning #off-policy #temporal-difference
- [[records/concepts/temporal-difference-learning]] — Learning predictions from the difference between successive estimates (bootstrapping) instead of waiting for the final outcome; the engine under Q-learning, DQN, and actor-critic  ·  #reinforcement-learning #temporal-difference #prediction #foundations
- [[records/concepts/markov-decision-process]] — The formal model underneath reinforcement learning: states, actions, transition probabilities, rewards, and a discount factor, with the Markov property  ·  #reinforcement-learning #theory #foundations #markov-decision-process
