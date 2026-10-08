# Lingo

Janela pequena que fica por cima durante calls em inglês:

- mostra o que os outros falam enquanto falam (cada palavra aparece ~1 s depois de dita), com o
  português saindo ~2 s atrás da fala, e explica expressões como *circle back* ou *touch base*;
- quando fazem uma pergunta (ou falam o seu nome), sugere 3 respostas em inglês: direta, completa
  e diplomática, cada uma com o sentido em português;
- no campo "Como digo…?" você escreve em português e recebe 3 jeitos de dizer em inglês.

Em call em português, o botão `EN→PT` da barra (ou `Ctrl+T`) troca para o modo `PT`: só transcreve,
sem tradução, e as respostas sugeridas vêm em português.

Feito para Linux com PipeWire. A janela é GTK4 nativa em Wayland; no Hyprland ela flutua, fica fixa
em todas as workspaces e tem atalhos globais.

## Como funciona

```
saída do sistema ─┬─pw-record──▶ gpt-live-transcribe (inglês, ao vivo) ──▶ janela, sugestões
                  └────────────▶ gpt-realtime-translate (português)  ──▶ janela
microfone        ──pw-record──▶ gpt-4o-transcribe                   ──▶ contexto das sugestões
```

Cada lado da conversa é um fluxo separado, então não precisa adivinhar quem falou. O inglês dos
outros chega palavra a palavra, e o português sai de um tradutor ao vivo que ouve o mesmo áudio,
~2 s atrás da fala, sem esperar a frase acabar. Cada fala ganha o seu pedaço da tradução embaixo:
a tradução só passa para a fala seguinte quando ela mesma faz uma pausa. As notas 💡 de expressões
vêm por frase, de uma chamada à parte ao `gpt-5.4-mini`.

Quem fecha a fala é o Lingo, depois de 700 ms sem voz. Falas seguidas da mesma pessoa ficam num
bloco só, e a fala em andamento tem uma barra azul à esquerda. Só os trechos com voz são enviados
(ao tradutor vão também 3 s de silêncio depois de cada fala: com menos, ele engole o final).

Com `translate = "gpt-5.4-mini"` em `[models]`, a tradução volta a ser frase a frase: o português
de cada frase só aparece ~2 s depois que ela termina. Custa metade, mas em frase longa a espera
chega a 8–10 s. A tradução ao vivo não aceita a lista de termos, então às vezes o jargão sai
traduzido.

Medido com fala gerada por TTS, do fim de cada palavra até ela aparecer na janela:

| | mediana | frase longa (p90) | pior |
|---|---|---|---|
| inglês | 1,15 s | 1,45 s | — |
| português, ao vivo | 2,2 s | 3,8 s | 4,3 s |
| português, frase a frase (`gpt-5.4-mini`) | 4,4 s | 8,6 s | 10,2 s |

No fim de uma pergunta, os dois jeitos de traduzir terminam ~2 s depois da última palavra.

A sugestão automática começa assim que o "?" aparece, sem esperar o silêncio: a primeira opção
fica completa ~2,3 s depois do fim da pergunta, e as três em ~3 s. Se a pessoa continua falando
depois da pergunta, ela é refeita quando a pessoa para; enquanto isso, as opções anteriores ficam
esmaecidas no painel.

### Precisão

O que mais dá errado em call de dev é o jargão em inglês no meio do português ("o pod" vira
"pode"). Três coisas resolvem:

- `keywords` em `[languages]`: nomes, siglas e termos do projeto, passados aos dois modelos;
- no modo `PT`, a fala dos outros é marcada como português **e** inglês;
- `transcribe_delay = "high"`: o modelo espera um pouco mais de contexto antes de escrever.

Em 8 falas de dev a ~230 palavras/min, com ruído e Opus a 16 kbps, o erro de palavras na fala dos
outros caiu de 3,5% para 1,1%, e o do microfone de 4,4% para 0,9%. Todos os termos técnicos
saíram certos, inclusive os que não estavam na lista. O custo é ~0,3 s a mais por palavra
(`low` deixa mais rápido, com o triplo de erros).

### Perguntar ao Claude

Para perguntas de fato sobre os projetos ("que versão da lib de pagamentos o checkout usa?"), o Lingo chama o
Claude Code (`claude -p`), que tem a memória dele e lê o código. A resposta aparece no painel
CLAUDE, que mostra o que ele está consultando enquanto procura:

- `SUPER+ALT+A`, `Ctrl+K` ou botão direito numa fala: pergunta sobre a última fala dos outros
  (ou sobre aquela fala);
- no campo de baixo, `?pergunta` + Enter pergunta o que você digitar (no modo `PT`, qualquer texto).

Se a resposta está na memória, chega em ~3–4 s. Com uma busca no código, em ~6–12 s, e quando ele
precisa investigar vários arquivos, em ~30 s. Ele roda a partir de `~` (a memória do Claude Code é
por pasta; mude em `[ask] cwd`), só com ferramentas de leitura (Read, Grep, Glob) e sem salvar a
sessão no histórico. O conteúdo da call vai para o Claude Code, não para a OpenAI.

## Instalar

Precisa de PipeWire (`pw-record`), GTK 4.14 ou mais novo, GLib 2.80 ou mais nova, Rust 1.92 ou
mais novo ([rustup](https://rustup.rs)) e uma chave da OpenAI. Para compilar:

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libssl-dev   # Ubuntu 24.04 / Debian
sudo pacman -S --needed base-devel gtk4 openssl                        # Arch / Omarchy
```

Depois:

```bash
tools/install.sh   # compila, instala em ~/.local/bin/lingo, cria o .desktop e as regras do Hyprland
mkdir -p ~/.config/lingo
(umask 077; read -rsp "chave da OpenAI: " k && echo "$k" > ~/.config/lingo/openai_key)
```

A chave também pode vir da variável `OPENAI_API_KEY`. Se algum atalho `SUPER+ALT` já fizer outra
coisa no seu Hyprland (no Omarchy, por exemplo), o `install.sh` o deixa comentado em
`~/.config/hypr/lingo.conf` e avisa.

## Usar

| | |
|---|---|
| `SUPER+ALT+L` | abre / mostra / esconde |
| `SUPER+ALT+R` | sugere respostas para a última fala |
| `SUPER+ALT+P` | pausa / retoma (pausar libera o microfone) |
| `SUPER+ALT+A` ou `Ctrl+K` | pergunta ao Claude sobre a última fala |
| clique numa fala | sugere respostas para aquela fala |
| botão direito numa fala | pergunta ao Claude sobre aquela fala |
| `?pergunta` + Enter no campo | pergunta ao Claude o que você digitou |
| clique numa opção ou `Ctrl+1..3` | copia a resposta |
| `Ctrl+T` ou o botão `EN→PT` / `PT` | troca entre traduzir e call em português |
| `Ctrl+R` `Ctrl+P` `Ctrl+M` `Ctrl+L` `Esc` | sugerir, pausar, microfone, limpar, fechar painel |

Pela linha de comando: `lingo --help`.

Configuração em `~/.config/lingo/config.toml` (modelo em `data/config.example.toml`). Log em
`~/.local/state/lingo/lingo.log`.

## Custos

A fala dos outros usa `gpt-live-transcribe` (US$ 0,017 por minuto de voz enviada; o silêncio não
vai) e `gpt-realtime-translate` (US$ 0,034 por minuto). O seu microfone usa `gpt-4o-transcribe`,
US$ 0,006 por minuto (`mic = "gpt-4o-mini-transcribe"` custa metade e erra o dobro). Numa call de
1 hora em que os outros falam metade do tempo, dá uns US$ 1,90; no máximo US$ 3,40 se falarem sem
parar. Com a tradução frase a frase (`translate = "gpt-5.4-mini"`), uns US$ 0,90.
Tradução e sugestões com `gpt-5.4-mini` são poucas centenas de tokens cada. O contador da barra
de cima mostra o custo estimado da transcrição e da tradução; com o mouse em cima, os minutos
enviados.

Para gastar ~6x menos na fala dos outros, ponha `transcribe = "gpt-4o-mini-transcribe"` em
`[models]`. O texto volta a aparecer só quando a pessoa para de falar.

## Bom saber

- **Bluetooth:** enquanto o microfone está aberto, o headset fica no perfil de chamada (som pior).
  Pausar ou fechar o Lingo libera o microfone.
- **Volume da call:** só conta como voz o que passa de `speech_level` (0,003). Se o medidor
  "eles" mexe mas nenhum texto aparece, baixe esse valor em `[audio]`; se ruído vira texto, suba.
- **Compartilhar tela:** a janela aparece se você compartilhar a tela inteira. Compartilhe só a
  janela da call.
- **Privacidade:** o áudio da call vai para a OpenAI. Veja se isso é permitido na empresa e no
  cliente antes de usar em reuniões com outras pessoas.
- **Fontes:** o Lingo usa um cache de fontconfig só seu (`~/.cache/lingo`), porque o Edge grava
  um cache em formato novo em `~/.cache/fontconfig` que bagunça apps com fontconfig mais antigo.
- **Tradução em português de Portugal:** o tradutor ao vivo só aceita `pt`, sem variante, e às
  vezes responde "Implementámos" em vez de "Implementamos". Não há como pedir pt-BR para ele.

## Código

Rust, com GTK4 ([gtk4-rs](https://gtk-rs.org)) na thread principal e o motor no
[tokio](https://tokio.rs), em threads próprias:

| | |
|---|---|
| `src/engine.rs` | o motor: recebe tudo numa fila só (comandos da janela, eventos da OpenAI) e manda eventos para a janela |
| `src/realtime.rs` | WebSockets da Realtime API (transcrição e tradução ao vivo), com reconexão |
| `src/audio.rs` | captura com `pw-record` e os portões de voz/silêncio |
| `src/llm.rs`, `src/prompts.rs`, `src/text.rs` | chamadas de chat em streaming, prompts e leitura das respostas |
| `src/ask.rs` | perguntas ao Claude Code |
| `src/ui/` | a janela e a linha de comando |

A primeira versão, em Python, está no branch `python`.

## Testes

```bash
cargo test
```

Para abrir uma segunda instância, sem mexer na que está aberta:
`LINGO_APP_ID=dev.pedro.LingoTest LINGO_CONFIG=/caminho/config.toml target/release/lingo`.

## Licença

MIT. Veja [LICENSE](LICENSE).
