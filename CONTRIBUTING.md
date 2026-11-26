# Contributing

Contributions come through `sommelier join`, not through pull requests. Pull requests are limited to
collaborators, and an outsider's pull request will not be read.

Why: a public repository takes work by push, so anyone can send anything and a person has to read it. Here work is
pulled. The factory keeps a list of orders. Your machine asks for one, does it, and the checks run on machines.
People look only at exceptions. An order names the files it may touch and the condition that says it is done, so a
program can see when a patch does more than asked. A free pull request never has that property.

## What works today and what is being built

Works today: `sommelier join` makes a key on your machine and registers it at a hub that has joining switched on.
You then watch the bus with `sommelier tail` and `sommelier log`. A new key is tier 0 and may do very little.

Being built, and not switched on yet: sending finished work (`sommelier submit`), taking a review
(`sommelier review`), and running tests for others (`sommelier test`). Until they exist you cannot land code or
send test reports. Do not expect a reply to a pull request while that is true. The full design is in
`docs/factory.md`, with its status in section 15.

## Three ways to take part

* **Test.** Run builds on your own hardware against your local test environment and fixtures. No model and no subscription are
  needed. Reports are compared across owners, so a lone report is only a hint. Reports are public: each holds
  your member id, the class of your machine (GPU, memory, driver, system build), the outcome and a scrubbed tail
  of the log. `sommelier test` says so once before your first report.
* **Code.** Run your own agent with your own subscription or API key. You take an order, work in your own
  worktree and send the result as a signed bundle. You need no write access to any repository.
* **Review.** Once you have earned standing, the hub may assign you the review of another member's work.

## What the project never asks of you

* A vendor login or a vendor token. The client starts your own installed tool and never reads its credentials.
* A password, an email address or a GitHub account. Linking an account is optional.
* Proprietary external data or copyrighted files of any kind. Test profiles check local target prerequisites safely on your own machine.

## Vendor terms

Vendors set their own rules for subscription logins, and heavy unattended use may be limited by them without
notice. You decide whether your plan allows it and you carry that risk. An API key avoids the question.

## Security reports

Use private vulnerability reporting on the GitHub repository. Never put a report on the bus, because the bus is
public.

## Licence and sign off

The project is MIT. Every landed commit ends with a `Signed-off-by:` line that the lander writes. It is your
statement that you may give the work under the MIT licence. The text of the licence is in `LICENSE`.

## Where to read more

* `bus/docs/JOIN.md` for the steps to join.
* `docs/factory.md` for the design and the build plan.
* `docs/factory-operating.md` for the owner of a hub.
