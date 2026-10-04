<p align="center">
  <img src=".github/assets/hex-icon.png" width="96" height="96" alt="HEX app icon" />
</p>

<h1 align="center">Hex</h1>

<p align="center">
  Segure o atalho, fale, solte: o texto aparece onde você está digitando.<br />
  Um fork enxuto do <a href="https://github.com/anomalyco/hex">HEX</a> que transcreve pelo OpenRouter.
</p>

O app faz uma coisa só, do jeito mais direto possível:

```
atalho → gravação → corte de silêncio → OpenRouter (com fallback) → colar
```

Ficaram de fora os modelos locais, os comandos de voz, o Voice Action, os
Modes/OpenCode, as reuniões, a API local, o SDK e a versão Linux. Sobraram
o atalho de ditado, o HUD, o History e uma página de Statistics.

## Instalar

Apple silicon, macOS 15 ou mais novo:

```sh
brew tap publi0/hex https://github.com/publi0/hex
brew install --cask publi0/hex/hex-openrouter
```

Atualizar: `brew upgrade --cask hex-openrouter`.

O app se chama **Hex** (bundle id `dev.publio.hex-openrouter`) e
guarda tudo em `~/Library/Application Support/hex-openrouter`. O identificador e o diretório
de dados permanecem os mesmos das versões anteriores. O build é assinado ad hoc e o cask
remove a quarentena; como a assinatura muda a cada versão, o macOS pode pedir
de novo Acessibilidade e Input Monitoring depois de um update.

## Primeiro uso

A tela de setup pede Microfone, Input Monitoring, Acessibilidade e a chave do
OpenRouter ([crie uma aqui](https://openrouter.ai/keys)). A chave vai para o
Keychain. Depois disso o atalho padrão é **Option**: segure para gravar,
solte para transcrever. Dois toques rápidos travam a gravação; um terceiro
termina. **Esc** cancela.

## Settings

- **OpenRouter API key.** Com uma chave salva, aparece "Key saved · …abcd"
  com **Test**, **Replace** e **Remove**. Se a chave estiver em texto puro no
  `openrouter.json`, **Move to Keychain** move para o Keychain.
- **Language.** Dica de idioma enviada ao modelo; Auto-detect deixa o modelo
  decidir.
- **Trim silence.** Corta silêncio no início e no fim e encurta pausas longas
  antes de enviar, então menos áudio é cobrado. Gravações sem fala não são
  enviadas.
- **Models.** Um modelo principal e até dois fallbacks, escolhidos no
  catálogo de speech-to-text do OpenRouter (dá para colar qualquer id).
  Qualquer erro (rate limit, timeout, erro do provedor, resposta vazia) passa
  para o próximo. **↑** reordena, **✕** remove.
- **Advanced.** Timeout por tentativa e total, tamanho máximo de trecho para
  áudio longo, espera máxima para repetir um 429, temperatura e URL da API.
- **Dictation, Paste last, Microphone, Application.** Atalhos, double-tap,
  colar o último ditado de novo, microfone, o que fazer com outros áudios
  durante o ditado, abrir no login, ícone no Dock e volume dos sons.

Tudo do OpenRouter fica em `openrouter.json`, relido a cada ditado:

```json
{
  "base_url": "https://openrouter.ai/api/v1",
  "transcription": {
    "models": ["openai/whisper-large-v3-turbo", "openai/gpt-4o-mini-transcribe"],
    "language": "auto",
    "trim_silence": true,
    "attempt_timeout_seconds": 30,
    "total_timeout_seconds": 90,
    "chunk_seconds": 120,
    "rate_limit_retry_max_wait_ms": 2000,
    "temperature": 0.0
  }
}
```

A chave também pode vir de `OPENROUTER_API_KEY` ou do campo `api_key` do
arquivo; nessa ordem, ambas têm prioridade sobre o Keychain.

## History e Statistics

O **History** guarda o texto colado, o app em foco, o modelo que respondeu e
a latência, os modelos que falharam antes e quanto áudio foi enviado depois do
corte de silêncio. A retenção padrão é de 7 dias; nunca guarda áudio.

**Statistics** soma, por dia, palavras, ditados, áudio gravado e enviado,
tokens e custo (como o OpenRouter informa em cada resposta), latência média,
quantas vezes precisou de fallback e por quê, por modelo. Os totais ficam em
`stats.json`, sem texto nem áudio.

## Privacidade

O áudio de cada ditado vai para o OpenRouter e para o provedor do modelo
escolhido. Nada de áudio é salvo localmente. O History guarda só texto e
metadados, e pode ser desligado ou limpo na própria tela.

## Desenvolvimento

```sh
cargo test
cargo clippy --all-targets -- -D warnings
fork/build-app.sh            # target/fork-app/Hex-<versão>.zip
```

O app só compila para macOS. Em outras plataformas, `cargo test` roda os
módulos portáveis (OpenRouter, History, Statistics). Para manter as
permissões do macOS entre builds locais, assine com uma identidade estável:
`FORK_CODESIGN_IDENTITY="Apple Development: …" fork/build-app.sh`.

Cada push numa branch roda `fork-check.yml` (fmt, clippy e testes no macOS).
Cada push em `main` roda `fork-release.yml`, que publica o release e atualiza
o cask em `Casks/`.

## Licença

MIT, como o [HEX](https://github.com/anomalyco/hex) original de Kit Langton.
Veja `LICENSE` e `THIRD_PARTY_NOTICES.md`.
