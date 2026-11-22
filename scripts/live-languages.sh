#!/bin/sh
# Compile and run a real program in many languages, each in its own container (needs network, a running engine).
# Usage: scripts/live-languages.sh        prints PASS/FAIL per language
D="${DOCKER:-docker}"
fail=0
t() { # name image script
  out=$($D run --rm "$2" sh -c "$3" 2>&1)
  case "$out" in
    *hello-from-$1*) echo "PASS $1" ;;
    *) echo "FAIL $1: $(echo "$out" | tail -3)"; fail=1 ;;
  esac
}
t python python:alpine 'python -c "print(\"hello-from-python\")"'
t node node:alpine 'node -e "console.log(\"hello-from-node\")"'
t bun oven/bun:alpine 'bun -e "console.log(\"hello-from-bun\")"'
t deno denoland/deno:alpine 'deno eval "console.log(\"hello-from-deno\")"'
t go golang:alpine 'cd /tmp && printf "package main\nimport \"fmt\"\nfunc main(){fmt.Println(\"hello-from-go\")}\n" > m.go && go run m.go'
t rust rust:alpine 'cd /tmp && printf "fn main(){println!(\"hello-from-rust\");}\n" > m.rs && rustc m.rs && ./m'
t java eclipse-temurin:21-jdk-alpine 'cd /tmp && printf "public class M{public static void main(String[] a){System.out.println(\"hello-from-java\");}}\n" > M.java && javac M.java && java M'
t php php:cli-alpine 'php -r "echo \"hello-from-php\n\";"'
t ruby ruby:alpine 'ruby -e "puts %q(hello-from-ruby)"'
t perl perl:slim 'perl -e "print qq(hello-from-perl\n)"'
t c alpine 'apk add -q gcc musl-dev && cd /tmp && printf "#include <stdio.h>\nint main(){puts(\"hello-from-c\");return 0;}\n" > m.c && gcc m.c -o m && ./m'
t cpp alpine 'apk add -q g++ && cd /tmp && printf "#include <iostream>\nint main(){std::cout<<\"hello-from-cpp\\n\";}\n" > m.cpp && g++ m.cpp -o m && ./m'
t lua alpine 'apk add -q lua5.4 && lua5.4 -e "print(\"hello-from-lua\")"'
t elixir elixir:alpine 'elixir -e "IO.puts(\"hello-from-elixir\")"'
t dotnet mcr.microsoft.com/dotnet/sdk:8.0-alpine 'export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1 && cd /tmp && dotnet new console -o app -n app >/dev/null && cd app && printf "System.Console.WriteLine(\"hello-from-dotnet\");\n" > Program.cs && dotnet run'
t kotlin zenika/kotlin:latest 'echo "println(\"hello-from-kotlin\")" > /tmp/m.kts && kotlinc -script /tmp/m.kts'
t r r-base 'Rscript -e "cat(\"hello-from-r\n\")"'
t haskell haskell:slim 'cd /tmp && echo "main = putStrLn \"hello-from-haskell\"" > m.hs && ghc m.hs -o m >/dev/null && ./m'
exit $fail
