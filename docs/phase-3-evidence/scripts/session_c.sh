#!/bin/zsh
cd /Users/mohitmanna/Desktop/Projects/Sum100
E=docs/phase-3-evidence/session-c
mkdir -p $E
echo "start $(date -u +%FT%TZ)" > $E/timeline.txt
NO_COLOR=1 ./target/release/sum100 dump --venue kalshi --prod --tickers KXBTCD-26SEP1417-T76999.99,KXBTCD-26SEP1417-T77499.99,KXBTCD-26SEP1417-T77749.99,KXBTCD-26SEP1417-T77999.99 --out data/phase3-c --seconds 600 --interval-ms 600000 --digest-out $E/live.digest > $E/live.stdout 2> $E/live.stderr &
PID=$!
sleep 40
kill -STOP $PID && echo "SIGSTOP pid=$PID $(date -u +%FT%TZ)" >> $E/timeline.txt
sleep 480
kill -CONT $PID && echo "SIGCONT pid=$PID $(date -u +%FT%TZ)" >> $E/timeline.txt
wait $PID
echo "exit $? $(date -u +%FT%TZ)" >> $E/timeline.txt
