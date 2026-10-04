<p align="center">
  <img src=".github/assets/hex-icon.png" width="96" height="96" alt="Hex app icon" />
</p>

<h1 align="center">Hex</h1>

<p align="center">
  Ditado por atalho para macOS, com transcrição via OpenRouter.
</p>

## Instalação rápida

**Mac com Apple silicon e macOS 15 ou mais novo**, com
[Homebrew instalado](https://brew.sh/).

No GitHub, passe o cursor sobre o bloco abaixo e clique no ícone de copiar
no canto superior direito. Cole tudo no Terminal e pressione **Return**:

```sh
brew tap publi0/hex https://github.com/publi0/hex &&
brew install --cask publi0/hex/hex-openrouter &&
open -a Hex
```

O bloco adiciona o tap, instala o aplicativo e abre o Hex. Depois, siga o
[primeiro uso](#primeiro-uso) para conceder as permissões e cadastrar sua chave.
Já tem o Hex instalado? Use os [comandos de atualização](#atualizar).

## Como funciona

Segure o atalho, fale e solte. O Hex transcreve o áudio e cola o resultado no
aplicativo em foco. Você escolhe o modelo e os fallbacks; o app cuida da
captura, do corte de silêncio e da ordem de entrega dos ditados.

```text
atalho → gravação → corte de silêncio → OpenRouter → colar
                                             ↓ erro
                                       próximo modelo
```

O app fica na barra de menus e reúne três telas: **Settings**, **History** e
**Statistics**. A transcrição exige conexão à internet e uma chave própria do
[OpenRouter](https://openrouter.ai/keys).

## Atualizar

Copie este bloco inteiro para atualizar o Hex e abri-lo:

```sh
brew update &&
brew upgrade --cask publi0/hex/hex-openrouter &&
open -a Hex
```

O aplicativo instalado se chama **Hex.app**. O pacote Homebrew continua se
chamando `hex-openrouter`, para manter o caminho de atualização das versões
anteriores. O bundle id `dev.publio.hex-openrouter` e o diretório de dados
também foram preservados.

Os releases são assinados ad hoc, sem notarização. O cask remove a quarentena
do aplicativo durante a instalação. Como a assinatura muda entre builds, o
macOS pode pedir novamente Acessibilidade e Input Monitoring após atualizar.

## Primeiro uso

1. Abra o **Hex** e conceda Microfone, Input Monitoring e Acessibilidade na
   tela de setup. As permissões são concedidas por você nos ajustes do macOS.
2. Cole sua chave do OpenRouter. O app a salva no Keychain e testa o acesso.
3. Em **Settings**, escolha o modelo principal, os fallbacks e o idioma.
4. Coloque o cursor onde quer escrever, segure **Option**, fale e solte.

O atalho é configurável. Com double-tap habilitado, dois toques rápidos travam
a gravação; a próxima pressão a encerra. **Esc** cancela a captura ou, quando
não há gravação ativa, o ditado pendente mais recente. Capturas com menos de
300 ms são descartadas.

O indicador flutuante mostra gravação e processamento. Você pode iniciar
outro ditado enquanto o anterior é transcrito: os resultados são colados na
ordem em que foram enviados. **Paste Last Dictation**, no menu, cola novamente
o último resultado da sessão; se houver uma captura ativa, ela é descartada.

## Settings

| Controle | Comportamento |
| --- | --- |
| **OpenRouter API key** | Uma chave salva mostra seus últimos quatro caracteres, com **Test**, **Replace** e **Remove**. A remoção pede confirmação. **Move to Keychain** migra uma chave que esteja no arquivo de configuração. |
| **Models** | Um modelo principal e até dois fallbacks pela interface. O picker consulta o catálogo de speech-to-text do OpenRouter e aceita IDs customizados. **↑** muda a ordem e **✕** remove um fallback. |
| **Language** | Envia uma dica de idioma ao modelo. **Auto-detect** deixa a identificação com o provedor. |
| **Trim silence** | Remove silêncio nas bordas e reduz pausas longas antes do envio. Usa uma heurística de energia do áudio; clipes classificados como silenciosos não são enviados. Pode ser desligado. |
| **Advanced** | URL da API, timeout por tentativa e por trecho, duração dos trechos, espera máxima para repetir um HTTP 429 e temperatura. |
| **Dictation / Paste last** | Atalhos e comportamento do double-tap. |
| **Microphone / Application** | Microfone, liberação do dispositivo enquanto ocioso, comportamento de outros áudios durante a gravação, sons, ícone no Dock e início no login. |

Idioma, trim e modelos são salvos ao mudar o controle. Os campos de
**Advanced** são aplicados com **Save** ou **Return**. A configuração é relida
antes de cada ditado, sem precisar reiniciar o app.

Qualquer falha de um modelo — timeout, erro de transporte, resposta vazia ou
erro HTTP — permite tentar o próximo. Um HTTP 429 com espera informada dentro
do limite configurado recebe uma tentativa adicional no mesmo modelo.
Gravações longas são divididas em trechos; cada trecho usa a mesma cadeia de
fallbacks e seu próprio limite total de tempo.

### Configuração em arquivo

As opções ficam em `~/Library/Application Support/hex-openrouter/openrouter.json`.
**Show file**, em Advanced, abre sua localização. Exemplo de formato; escolha
os IDs de modelo no catálogo da interface:

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

A chave é resolvida nesta ordem: `OPENROUTER_API_KEY`, campo `api_key` do
arquivo e Keychain. Prefira o Keychain para evitar uma chave em texto puro.
O arquivo pode conter mais fallbacks que os dois editáveis pela interface.

## History e Statistics

**History** registra os ditados colados com sucesso: texto, aplicativo em
foco, duração, latência, modelos utilizados, fallbacks e corte de silêncio.
Tem busca, cópia e controles de retenção e limpeza. A retenção padrão é de
**7 dias**, com limites adicionais de quantidade e tamanho. A atualização da
versão 2.x preserva o texto e os relatórios de transcrição existentes.

**Statistics** reúne palavras, ditados, áudio gravado e enviado, clipes
silenciosos, tokens, custo informado pelo OpenRouter, latência de transcrição,
fallbacks e erros por tipo e modelo. Não armazena texto nem áudio. A latência
mostrada corresponde ao processamento da transcrição, sem o tempo na fila
ou na colagem. Um ditado com vários trechos pode contar para mais de um modelo.

Em **Models and latency**, cada modelo mostra sua latência média e a
quantidade de respostas medidas no período. A média usa apenas requisições
bem-sucedidas, incluindo o tempo de rede; não inclui a fila local, tentativas
que falharam nem a espera entre retries. Cada trecho de áudio respondido
fornece uma medição. Os registros anteriores à versão 3.0.1 continuam
preservados, mas aparecem sem latência por modelo até haver novas medições.

Os períodos são **Today**, **7 days**, **30 days** e **All time**, calculados
por datas locais. São mantidos até 400 dias com registros; **All time** soma
os dias retidos e seu gráfico mostra os últimos 30 dias. **Reset** limpa as
estatísticas após confirmação. Tokens e custos dependem dos dados de uso
retornados pelo provedor.

## Dados e privacidade

O áudio enviado passa pelo OpenRouter e pelo provedor do modelo escolhido.
O Hex não salva arquivos de áudio localmente. As regras de retenção desses
serviços são independentes dos controles locais do app.

Os arquivos locais ficam em `~/Library/Application Support/hex-openrouter`:

| Local | Conteúdo |
| --- | --- |
| `settings.json` | Preferências do aplicativo e atalhos. |
| `openrouter.json` | Modelos, idioma e parâmetros de transcrição; a chave só aparece se configurada em texto puro. |
| `history.json` | Texto dos ditados e metadados, conforme a retenção escolhida. |
| `stats.json` | Totais diários, sem texto, áudio ou corpos de respostas de erro. |
| `logs/live.ndjson` e `logs/process.log` | Eventos e diagnósticos. O log de eventos inclui texto colado e aplicativo em foco; erros podem incluir detalhes retornados pelo provedor. |

Desligar ou limpar o **History** não desliga nem limpa os logs. Considere seu
conteúdo antes de compartilhá-los para diagnóstico. A chave salva no Keychain
não é colocada nos argumentos dos processos de rede ou de acesso ao Keychain.

## Desenvolvimento

O aplicativo é escrito em Rust com GPUI. O build para macOS requer Rust
stable, Python 3.11 ou mais novo e Xcode com as ferramentas de compilação
Metal disponíveis. Antes de cada commit, rode a validação local completa:

```sh
scripts/check-local.sh
scripts/build-app.sh
```

O empacotamento gera `target/app/Hex.app` e
`target/app/Hex-<versão>.zip`. Para builds locais com uma identidade de
assinatura estável, configure `HEX_CODESIGN_IDENTITY` antes de executar o
script.

Previews isolados para conferir a interface, sem gravação ou acesso à
configuração e às credenciais reais:

```sh
cargo run -- preview settings
cargo run -- preview history
cargo run -- preview statistics
cargo run -- preview onboarding
cargo run -- preview dictation-hud
```

Somente macOS executa o aplicativo. Em outras plataformas, o crate permite
rodar os testes dos módulos portáveis, como OpenRouter, History e Statistics;
isso não constitui uma versão Linux do app. Veja [AGENTS.md](AGENTS.md) para
os módulos e contratos internos.

### Validação local e releases

Formatação, Clippy, testes unitários e de integração são executados **na
máquina de desenvolvimento, antes de cada commit**, pelo
[`scripts/check-local.sh`](scripts/check-local.sh), junto com os testes da
automação de publicação. A suíte completa roda uma vez no perfil de
desenvolvimento. Use `scripts/check-local.sh --release` para conferir também
o perfil otimizado quando alterar otimizações ou investigar uma diferença
exclusiva de release. Corrija falhas antes de comitar ou enviar mudanças.

Mantenha o cache do Cargo e o diretório `target/` entre execuções; não é
necessário fazer `cargo clean`. O script informa o tempo de cada etapa e
salva os tempos de compilação em `target/cargo-timings/`.

O GitHub Actions é usado **somente para releases**: compilar o aplicativo,
empacotar, assinar, publicar e atualizar o cask. Ele não executa testes nem
substitui a validação local. Essa divisão está definida em
[AGENTS.md](AGENTS.md).

A versão vem do `Cargo.toml`, com a entrada correspondente no `Cargo.lock`.
Ao chegar à `main`, uma versão ainda não publicada dispara o
[workflow de release](.github/workflows/release.yml). Por exemplo:

- Versão do pacote: `3.0.0`.
- Tag: `v3.0.0`.
- Release: **Hex 3.0.0**.
- Arquivo: `Hex-3.0.0.zip`.

Novos commits sem mudança de versão não criam outro release. Para publicar
a próxima versão, altere explicitamente o número, rode os checks locais e
integre a mudança. Não há incremento automático nem sufixo com número da
execução. Reexecutar o workflow pode concluir uma publicação interrompida ou
reparar o cask, preservando os releases já publicados.

## Escopo do fork

Este projeto deriva do [HEX](https://github.com/anomalyco/hex) de Kit Langton
e mantém o fluxo de ditado com transcrição remota. Foram removidos os modelos
locais, Voice Commands, Voice Action, Modes/OpenCode, reuniões, API local,
SDK, Linux, cleanup adicional do OpenRouter, Sparkle e sincronização
automática com o upstream.

## Licença

[MIT](LICENSE). Dependências e atribuições estão em
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
