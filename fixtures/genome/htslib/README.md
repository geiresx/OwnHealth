# htslib-made raw files (synthetic)

Alex Rivera's synthetic calls VCF and gVCF as `backend/src/ownhealth/genome/synthetic.py`
`alex_raw_files()` writes them with pysam (`pysam.tabix_index`, htslib's bgzip and tabix). The Rust
genome tests read them to check that noodles answers the same lookups from htslib's own `.tbi`
files, the kind every real import brings. Regenerate with:

```sh
cd backend && .venv/bin/python -c "from pathlib import Path; from ownhealth.genome import synthetic; synthetic.alex_raw_files(Path('../fixtures/genome/htslib'))"
```
