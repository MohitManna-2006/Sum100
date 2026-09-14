#!/bin/zsh
cd /Users/mohitmanna/Desktop/Projects/Sum100
E=docs/phase-3-evidence/session-b
T=KXBTCD-26SEP1417-T76999.99,KXBTCD-26SEP1417-T77499.99,KXBTCD-26SEP1417-T77749.99,KXBTCD-26SEP1417-T77999.99
echo "start $(date -u +%FT%TZ)" > $E/timeline.txt
./target/release/sum100 dump --venue kalshi --prod --tickers $T --out data/phase3-b --seconds 330 --interval-ms 60000 --digest-out $E/live.digest > $E/live.stdout 2> $E/live.stderr &
PID=$!
sleep 60
kill -STOP $PID && echo "SIGSTOP pid=$PID $(date -u +%FT%TZ)" >> $E/timeline.txt
sleep 100
kill -CONT $PID && echo "SIGCONT pid=$PID $(date -u +%FT%TZ)" >> $E/timeline.txt
wait $PID
echo "exit $? $(date -u +%FT%TZ)" >> $E/timeline.txt
