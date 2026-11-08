<h1 align="center">OwnHealth</h1>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/GeiserX/OwnHealth?style=flat-square" alt="License"></a>
</p>

OwnHealth is a self-hosted health data layer you own. It keeps your lab results, medical records from any country, wearable data and genome findings in one place you run yourself. You read it in two ways: a web page, and an MCP server that lets an agent such as Claude Code answer "how is my health right now?" with a source behind every value.

There is no chat agent and no AI model inside OwnHealth. The agent is yours, and it sees only what its token allows.

You run it yourself.

<p align="center">
  <img src="docs/mockup/screenshots/01-dashboard.png" alt="OwnHealth: the family dashboard in the dark theme, with synthetic data" width="800">
</p>

**Status: pre-alpha.** Expect breaking changes before 1.0. Upgrades keep your data: see the [changelog](CHANGELOG.md).

## What it does

In v0.1:

- One dashboard for the whole family, one profile per person, with OK, Watch, Out, Stale or No data per body system
- Lab reports read from PDFs into trendable values, each linked to its page and position on the page; anything uncertain waits in a review queue
- FHIR and IPS bundles, the Apple Health export, and manual entry
- Genome findings from [OwnGenome](https://github.com/GeiserX/OwnGenome): drug response, carrier status and risk variants, joined with your medications and labs
- A drug check that quotes the CPIC guideline for your genotype next to your current medications and relevant labs
- An MCP server with 8 tools, scoped per person and per token, with a citation behind every answer
- Passkeys, an append-only audit log, a full export, and restic backups with a scripted restore test
- Dark theme, Spanish and English

Limits in v0.1. Lab PDFs are read with generic table rules plus one template, for the fictional LabNorte lab. Real lab layouts come later as contributed templates. The LOINC set is a curated subset of 202 codes, so a test outside it waits in review.

Later: email and folder inboxes, local OCR, imaging (DICOM), more wearables and US patient portals. See the [roadmap](docs/ROADMAP.md).

## Quick start

You need Docker with Compose v2. OwnHealth runs as one app container, a single Rust binary with the web UI built in (a 348 MB image), next to PostgreSQL 17.

```sh
git clone https://github.com/GeiserX/OwnHealth.git
cd OwnHealth
cp .env.example .env
openssl rand -hex 32     # paste as OWNHEALTH_DB_PASSWORD in .env
openssl rand -hex 32     # paste as OWNHEALTH_APP_DB_PASSWORD in .env
openssl rand -hex 48     # paste as OWNHEALTH_SECRET_KEY in .env
docker compose up -d
```

Open http://localhost:8080, create your account with a passkey, and import a lab report under **Import & review**.

Connect Claude Code with a token from **MCP & access**:

```sh
export OWNHEALTH_PAT=oh_pat_...   # the token you copied
claude mcp add --transport http ownhealth http://localhost:8080/mcp \
  --header "Authorization: Bearer ${OWNHEALTH_PAT}"
```

The app listens on `127.0.0.1` only. For HTTPS on your network, backups and restores, see [deploy/README.md](deploy/README.md). The full walk-through, with a demo family to look around first, is in [docs/quickstart.md](docs/quickstart.md).

## Intended purpose

OwnHealth is for personal record keeping and for showing values computed from published formulas, each with its source. It is information, not diagnosis. It does not tell anyone what to take or stop; pharmacogenomic results quote the published guideline and link it.

## Documentation

- [Quick start](docs/quickstart.md) and [deploy, back up, restore](deploy/README.md)
- [MCP tool reference](docs/mcp-tools.md): every tool, with example responses
- [Plan](docs/PLAN.md): the problem, principles and decisions
- [Roadmap](docs/ROADMAP.md): phases, deliverables and exit criteria
- [Mockup](docs/mockup/README.md): every screen with synthetic data
- Design: [architecture](docs/design/architecture.md), [ingestion](docs/design/ingestion.md), [connectors](docs/design/connectors.md), [MCP](docs/design/mcp.md), [genomics](docs/design/genomics.md), [security, privacy and threat model](docs/design/security-privacy.md)
- [Rivals](docs/research/rivals.md)
- [Contributing](CONTRIBUTING.md): how to add a lab template or an alias
- [Security policy](SECURITY.md)

## Related projects

[OwnGenome](https://github.com/GeiserX/OwnGenome) produces the genome findings OwnHealth shows. It turns a whole genome or a consumer chip into drug-response, carrier and risk findings; OwnHealth ingests them and joins them with your medications and labs.

## License

[AGPL-3.0-or-later](LICENSE)
