# Hex OpenRouter (fork de anomalyco/hex)

Este fork adiciona transcrição via [OpenRouter](https://openrouter.ai) ao HEX,
com uma cadeia de modelos de fallback e um cleanup opcional por LLM. Os modelos
locais, o Voice Action e todo o resto do upstream continuam iguais.

## Instalar

```sh
brew tap publi0/hex https://github.com/publi0/hex
brew install --cask publi0/hex/hex-openrouter
```

Para atualizar: `brew upgrade --cask hex-openrouter`.

O app se chama **Hex OpenRouter**, tem bundle id `dev.publio.hex-openrouter` e
guarda tudo em `~/Library/Application Support/hex-openrouter`. Por isso ele
convive com o HEX oficial sem compartilhar configurações e não recebe os
updates do Sparkle do upstream.

O build é assinado ad hoc, sem notarização. O cask remove a quarentena após a
instalação. Como a assinatura muda a cada build, o macOS pode pedir de novo
as permissões de Acessibilidade e Input Monitoring depois de um update.

## Configurar

Tudo é configurado em **Settings**, nas seções **OPENROUTER** e
**OPENROUTER CLEANUP**, logo abaixo de Dictation:

1. **API key:** cole a chave e clique em **Save key**. Ela vai para o
   Keychain; o campo é limpo em seguida e só os quatro últimos caracteres
   aparecem. **Test key** consulta o OpenRouter e mostra o uso e o limite.
   **Remove** apaga a chave do Keychain.
2. **Modelos e limites:** edite os campos e clique em **Save**. **Revert**
   descarta as edições, **Defaults** carrega os valores padrão e **Show file**
   mostra o `openrouter.json` no Finder.
3. Em **Local transcription**, escolha **OpenRouter** e clique em **Use**. Ele
   já é o modelo padrão numa instalação nova.

Também dá para configurar sem a interface. A chave pode ficar no Keychain
(`security add-generic-password -s hex-openrouter -a openrouter -w`), na
variável `OPENROUTER_API_KEY` ou no campo `api_key` do arquivo. O arquivo
`~/Library/Application Support/hex-openrouter/openrouter.json` é criado no
primeiro uso e relido a cada ditado, então não precisa reiniciar o app.

```json
{
  "base_url": "https://openrouter.ai/api/v1",
  "transcription": {
    "models": [
      "openai/whisper-large-v3-turbo",
      "openai/gpt-4o-mini-transcribe",
      "mistralai/voxtral-mini-transcribe"
    ],
    "attempt_timeout_seconds": 30,
    "total_timeout_seconds": 90,
    "chunk_seconds": 120,
    "rate_limit_retry_max_wait_ms": 2000,
    "temperature": 0.0
  },
  "cleanup": {
    "enabled": false,
    "models": ["openai/gpt-4o-mini", "google/gemini-2.5-flash"],
    "timeout_seconds": 15
  }
}
```

- **Fallback:** os modelos de `transcription.models` são tentados em ordem.
  Qualquer erro passa para o próximo: falha de rede, timeout, HTTP não 2xx,
  JSON inválido ou resposta sem texto. Um 429 com `Retry-After` de até
  `rate_limit_retry_max_wait_ms` é repetido uma vez no mesmo modelo. Se todos
  falharem, o ditado mostra a lista de erros por modelo.
- **Idioma:** o idioma escolhido em Settings é enviado ao OpenRouter. "Auto"
  deixa o provedor detectar.
- **Áudio longo:** é cortado em trechos de até `chunk_seconds`, no ponto mais
  silencioso perto do limite, e os textos são concatenados.
- **Cleanup:** com `cleanup.enabled: true`, cada ditado passa por um modelo de
  texto antes dos Modes, com a mesma lógica de fallback. Se tudo falhar, cola
  o texto bruto. O History guarda o texto bruto e o final. Defina
  `cleanup.prompt` para trocar o prompt padrão.

Lista de modelos de STT disponíveis:
<https://openrouter.ai/api/v1/models?output_modalities=transcription>.

## Como o fork fica perto do upstream

Todo o código novo está em `src/openrouter/`. Ele entra no app pela feature
`openrouter` do Cargo (`cargo build --features openrouter`). Sem a feature, o
binário se comporta exatamente como o upstream e a suíte de testes do upstream
passa sem mudanças.

Arquivos do upstream tocados (poucas linhas cada):

| Arquivo | Mudança |
| --- | --- |
| `Cargo.toml` | declara a feature `openrouter` |
| `src/main.rs` | `mod openrouter;` |
| `src/transcription_models.rs` | entrada `OpenRouter` no catálogo, seleção padrão e choices do fork |
| `src/transcription.rs` | variante `Transcriber::OpenRouter` |
| `src/local_api.rs` | readiness do runtime remoto |
| `src/parakeet.rs` | cleanup opcional antes dos Modes |
| `src/app_paths.rs` | diretório de dados próprio do fork |
| `src/app_window.rs` | embute a seção OpenRouter no Settings (3 linhas) |

Arquivos exclusivos do fork: `src/openrouter/`, `fork/`, `Casks/`, `FORK.md` e
`.github/workflows/fork-*.yml`.

## Sincronizar com o upstream

**Automático:** `fork-sync.yml` roda todo dia. Ele faz merge de
`anomalyco/hex@main` em `main` e, se houve mudança, dispara o release e
atualiza o cask. O job falha, e o GitHub te avisa por e-mail, em dois casos:

- **Conflito.** Resolva localmente.
- **O upstream mudou arquivos em `.github/workflows/`.** O token do Actions não
  pode fazer push deles. Use o botão **Sync fork** no GitHub ou faça o merge
  localmente.

**Manual:**

```sh
git remote add upstream https://github.com/anomalyco/hex.git  # uma vez
git fetch upstream
git merge upstream/main
cargo test --bin voice-control                                   # suíte do upstream
cargo test --features openrouter --bin voice-control -- openrouter::
git push origin main                                             # dispara o release
```

Num conflito, quase sempre basta manter a mudança do upstream e reaplicar a
linha do fork, marcada com `// Fork:` ou `crate::openrouter::` no código.

## Build local

```sh
./scripts/setup.sh           # Moonshine (Commands) e SDK, como no upstream
fork/build-app.sh            # gera target/fork-app/Hex-OpenRouter-<versão>.zip
```

Para manter as permissões do macOS entre builds, assine com uma identidade
estável: `FORK_CODESIGN_IDENTITY="Apple Development: …" fork/build-app.sh`.
