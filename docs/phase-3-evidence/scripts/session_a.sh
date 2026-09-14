#!/bin/zsh
cd /Users/mohitmanna/Desktop/Projects/Sum100
E=docs/phase-3-evidence/session-a
T=KXBTCD-26SEP1417-T76999.99,KXBTCD-26SEP1417-T77499.99,KXBTCD-26SEP1417-T77749.99,KXBTCD-26SEP1417-T77999.99
echo "start $(date -u +%FT%TZ)" > $E/timeline.txt
./target/release/sum100 dump --venue kalshi --prod --tickers $T --out data/phase3-a --seconds 180 --interval-ms 60000 --digest-out $E/live.digest > $E/live.stdout 2> $E/live.stderr
echo "exit $? $(date -u +%FT%TZ)" >> $E/timeline.txt
