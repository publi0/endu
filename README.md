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
aplicativo onde você começou a gravar, se ele continuar em foco ao concluir.
Você escolhe o modelo e os fallbacks; o app cuida da
captura, do corte de silêncio e da ordem de entrega dos ditados.

```text
atalho → gravação → corte de silêncio → OpenRouter → colar
                                             ↓ erro
                                       próximo modelo
```

O app fica na barra de menus e reúne quatro telas: **Settings**, **Models**,
**History** e **Statistics**. A transcrição exige conexão à internet e uma chave própria do
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

As versões até 3.0.4 usam assinatura ad hoc, que muda de identidade a cada
build e pode fazer o macOS pedir permissões novamente. A partir da 3.0.5,
os releases usam um certificado próprio persistente. A migração exige uma
nova autorização; as versões seguintes preservam a identidade de assinatura.
Essa continuidade é verificada com duas builds diferentes nos testes locais.
Na atualização real de 3.0.5 para 3.0.6, Microfone, Input Monitoring e
Acessibilidade permaneceram autorizados sem novos pedidos.
Outro usuário/Mac, recursos novos e cada novo alvo de Automação, como Music,
Spotify ou VLC, ainda podem exigir autorizações próprias.

O certificado próprio não é um Developer ID e não oferece notarização da
Apple. O cask mantém a remoção de quarentena usada nas versões anteriores.
A instalação não adiciona certificados confiáveis ao sistema, altera o TCC
ou concede permissões por conta própria.

## Primeiro uso

1. Abra o **Hex** e conceda Microfone, Input Monitoring e Acessibilidade na
   tela de setup. As permissões são concedidas por você nos ajustes do macOS.
2. Cole sua chave do OpenRouter. O app a salva no Keychain e testa o acesso.
3. Em **Models**, escolha o modelo principal, os fallbacks e o idioma.
4. Coloque o cursor onde quer escrever, segure **Option**, fale e solte.

O atalho é configurável. Com double-tap habilitado, dois toques rápidos travam
a gravação; a próxima pressão a encerra. **Esc** cancela a captura ou, quando
não há gravação ativa, o ditado pendente mais recente. Capturas com menos de
300 ms são descartadas.

O indicador flutuante mantém a cápsula vermelha durante a gravação e a esfera
azul na transcrição. Se o microfone precisar abrir, a mesma cápsula aparece
apagada até o dispositivo estar pronto. Com o microfone já aberto, entra
direto no vermelho, sem espera adicional. Você pode iniciar
outro ditado enquanto o anterior é transcrito: os resultados são colados na
ordem em que foram enviados. **Paste Last Dictation**, no menu, cola novamente
o último resultado da sessão; se houver uma captura ativa, ela é descartada.

Se outro aplicativo estiver em foco ao concluir, o Hex não altera o clipboard
nem cola automaticamente. O aviso **Dictation ready** aparece por alguns
segundos, e **Paste Last Dictation (ready)** fica disponível no menu. Coloque
o cursor no destino e use essa ação para inserir o texto. O resultado fica
apenas na memória até outro ditado substituí-lo ou o app encerrar; entra no
History somente depois da colagem. A proteção identifica o aplicativo,
não a janela ou o campo dentro dele.

Os seletores de microfone, canal, idioma, modelo e retenção do History aceitam
teclado: **Tab** dá foco, **Enter** abre ou confirma, as **setas** percorrem as
opções e **Esc** fecha. Confirmações e erros de salvamento aparecem junto ao
controle alterado. Durante uma operação com a chave, ações incompatíveis
ficam indisponíveis até ela terminar.

## Models

A tela **Models**, na barra lateral, concentra a chave, o idioma, os modelos
e os parâmetros avançados de transcrição pelo OpenRouter.

| Controle | Comportamento |
| --- | --- |
| **OpenRouter API key** | Uma chave salva mostra seus últimos quatro caracteres, com **Test**, **Replace** e **Remove**. A remoção pede confirmação. **Move to Keychain** migra uma chave que esteja no arquivo de configuração. |
| **Models** | Um modelo principal e até dois fallbacks pela interface. O picker consulta o catálogo de speech-to-text do OpenRouter e aceita IDs customizados. **↑** muda a ordem e **✕** remove um fallback. |
| **Language** | Envia uma dica de idioma ao modelo. **Auto-detect** deixa a identificação com o provedor. |
| **Advanced** | URL da API, timeout por tentativa e por trecho, duração dos trechos, espera máxima para repetir um HTTP 429 e temperatura. |

Idioma e modelos são salvos ao mudar o controle. Os campos de
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

## Settings

As preferências gerais do aplicativo ficam em **Settings**:

| Controle | Comportamento |
| --- | --- |
| **Dictation / Paste last** | Atalhos e comportamento do double-tap. |
| **Microphone / Application** | Microfone, liberação do dispositivo enquanto ocioso, comportamento de outros áudios durante a gravação, sons, ícone no Dock e início no login. |
| **Microphone / Input channel** | Mantém a mistura atual por padrão. Em interfaces com vários canais, permite escolher explicitamente o canal do microfone. A escolha fica vinculada ao dispositivo. |
| **Microphone / Input levels** | Mostra RMS, pico e avisos de sinal muito baixo, ausência de sinal ou possível clipping na última gravação analisada. |
| **Microphone / Trim silence** | Remove silêncio nas bordas e reduz pausas longas antes do envio. Usa uma heurística de energia do áudio; clipes classificados como silenciosos não são enviados. Pode ser desligado. |

O **Trim silence** é salvo assim que você muda o controle. Avisos e atalhos
para conceder permissões do macOS também aparecem nessa tela.

A escolha do canal não muda o dispositivo selecionado: **Automatic** continua
automático. Se o canal salvo deixar de existir no dispositivo, o app volta à
mistura e mostra um aviso. A troca é aplicada entre gravações; os diagnósticos
identificam o dispositivo e o canal usados no clipe analisado.

Os níveis são medidos depois da conversão para 16 kHz e antes do corte de
silêncio. São uma indicação do sinal enviado para transcrição, não uma
medição calibrada do hardware. Ficam apenas na memória da sessão, sem guardar
o áudio. Não há normalização automática, AGC ou filtro de voz adicional.

Em **While dictating → Mute**, o volume desce e volta em cerca de **120 ms**
por transição. O fade roda fora da captura e pode inverter o sentido se outro
ditado começar logo em seguida. Se detectar uma mudança manual de volume ou
mute, o Hex deixa de controlar o nível para preservar sua escolha. A opção
**Pause media** mantém seu comportamento de pausar e retomar os players.

O sinal sonoro de início tem um reforço de volume para facilitar perceber
quando a gravação começou. O controle geral continua valendo, inclusive
quando os sons estão desligados; o sinal de término mantém seu volume.

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
`target/app/Hex-<versão>.zip`. Para empacotar localmente, configure
`HEX_CODESIGN_IDENTITY` com o certificado persistente e tenha sua chave no
Keychain. O script rejeita assinaturas ad hoc e certificados diferentes do
PEM público fixado no repositório. Para trabalhar na interface sem essa chave,
use os previews isolados abaixo. Não substitua o aplicativo instalado por
uma build de desenvolvimento. `--prepare` gera somente o bundle intermediário,
que ainda precisa da etapa de assinatura e não deve ser instalado.

No GitHub, `scripts/code_signing.py` usa os secrets `HEX_SIGNING_P12_BASE64`
e `HEX_SIGNING_P12_PASSWORD`. O certificado público correspondente fica em
`app/release-signing.pem`; a chave privada nunca entra no repositório. O
runner compila sem os secrets e só depois importa a identidade em um keychain
temporário para assinar. As senhas passam pela entrada padrão, sem aparecer
nos argumentos dos processos. O keychain é removido antes do empacotamento,
inclusive quando a assinatura falha normalmente. O arquivo baixado do GitHub
também tem sua assinatura conferida antes da publicação. Sem os secrets ou com outra identidade,
o release falha em vez de voltar à assinatura ad hoc. Preserve essa identidade
entre publicações: uma troca de certificado pode exigir novas permissões.
Mantenha um backup criptografado da identidade completa (certificado e chave
privada, em P12), com a senha guardada separadamente em um gerenciador de
senhas. O PEM público não permite reconstruir a chave. Não renove recriando
um certificado com o mesmo nome: o fingerprint muda mesmo ao reutilizar a
chave. Planeje a rotação antes do vencimento e uma nova autorização do usuário.

Previews isolados para conferir a interface, sem gravação ou acesso à
configuração e às credenciais reais:

```sh
cargo run -- preview settings
cargo run -- preview models
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
