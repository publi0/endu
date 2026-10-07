<p align="center">
  <img src=".github/assets/hex-icon.png" width="96" height="96" alt="Hex app icon" />
</p>

<h1 align="center">Hex</h1>

<p align="center">
  Ditado por atalho para macOS, com OpenRouter, OpenAI, Deepgram e ElevenLabs Scribe.
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

Dê um toque no atalho para começar a gravar e aperte novamente para encerrar.
Se preferir, mantenha o atalho pressionado e solte para encerrar. O Hex
transcreve o áudio e cola o resultado no
aplicativo onde você começou a gravar, se ele continuar em foco ao concluir.
Você escolhe o modelo e os fallbacks; o app cuida da
captura, do corte de silêncio e da ordem de entrega dos ditados.

```text
atalho → gravação / streaming → provider → formatação local → colar
                                  ↓ erro
                             próximo modelo
```

O app fica na barra de menus e reúne **Settings**, **Microphone**, **Providers**,
**Models**, **Post-processing**, **HUD**, **History** e **Statistics**. A transcrição
exige conexão à internet e uma chave própria de pelo menos um provider configurado.

## Tamanho em relação ao original

Comparação entre as releases para Apple silicon do
[Hex original 2.1.24](https://github.com/anomalyco/hex/releases/tag/app-v2.1.24)
e deste [Hex 3.0.7](https://github.com/publi0/hex/releases/tag/v3.0.7):

| Medida | Original 2.1.24 | Hex 3.0.7 | Redução |
| --- | ---: | ---: | ---: |
| Pacote de download | 34,88 MB | 8,97 MB | **25,91 MB · 74,3%** |
| Linhas nos arquivos do aplicativo | 62.623 | 29.140 | **33.483 linhas · 53,5%** |

O `Hex.app` da versão 3.0.7, descompactado, soma **19,41 MB** em arquivos.

**Critério da medição:** MB corresponde a 1.000.000 de bytes. O pacote
original é um DMG e o nosso é um ZIP; a diferença de formato e compressão
também influencia o tamanho do download. Dados do usuário e modelos
baixados separadamente não entram nessas medidas.

A contagem de linhas usa os arquivos de código em `src/`, `native/`,
`tests/` e `build.rs` das duas versões publicadas. Inclui testes, comentários
e linhas vazias; exclui documentação, site, SDK e dependências externas.
Os números se referem especificamente a essas versões, sem alterações em
desenvolvimento.

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
2. Em **Providers**, conecte OpenRouter, OpenAI, Deepgram ou ElevenLabs. Cada
   chave fica em uma entrada separada do Keychain. No setup, **Choose another
   provider** abre essa tela.
3. Em **Models**, escolha o principal e os fallbacks. Os ajustes de cada modelo
   ficam em **Providers** e são salvos automaticamente.
4. Coloque o cursor onde quer escrever, toque **Option**, fale e pressione
   **Option** novamente para encerrar. Segurar e soltar também funciona.

Os dois atalhos são configuráveis e têm **Reset** para voltar ao padrão:
**Option** para ditar e **Option + Shift + V** para colar o último ditado.
Reset também reativa o Paste Last quando estiver em **Off**, e avisa se o
padrão estiver ocupado pelo outro atalho.

O modo padrão **Tap or hold** trava a gravação quando
você solta o atalho antes de 300 ms; uma nova pressão encerra. Ao segurá-lo por
300 ms ou mais, soltar encerra o ditado. **Hold only** mantém só o gesto de
segurar; **Double tap** preserva a opção anterior de dois toques para travar.
Configurações antigas que desativaram explicitamente o double-tap mantêm
**Hold only**. **Esc** cancela a captura ou o ditado pendente mais recente.
Áudios com menos de 300 ms continuam sendo descartados.

A opção **Enter to paste and send**, desligada inicialmente, permite pressionar
**Enter** durante a gravação travada para encerrar, transcrever, colar e enviar
um Enter ao campo. Ela não atua enquanto você segura o atalho; **Shift+Enter**
continua tendo seu comportamento normal. O texto pode ser enviado como uma
mensagem em aplicativos de conversa.

Esse Enter fica vinculado àquela gravação e ao aplicativo de destino. Uma
nova tecla ou clique durante a espera impede o envio automático e deixa o
resultado disponível em **Paste Last**. Se o foco mudar depois da colagem,
o texto já inserido permanece, mas o Enter é omitido. Erros, resultado vazio
e cancelamento não enviam Enter. **Paste Last** nunca repete esse envio.

Em **Double tap**, **Double-tap timing** oferece Short (200 ms), Normal
(300 ms) e Tolerant (450 ms), sem alterar a duração mínima da captura nem
o limiar de 300 ms do modo Tap or hold.

No padrão, o indicador flutuante mantém a cápsula vermelha durante a gravação e a esfera
azul na transcrição. Se o microfone precisar abrir, a mesma cápsula aparece
apagada até o dispositivo estar pronto. Com o microfone já aberto, entra
direto no vermelho, sem espera adicional. Você pode iniciar
outro ditado enquanto o anterior é transcrito: os resultados são colados na
ordem em que foram enviados. **Paste Last Dictation**, no menu, cola novamente
o último resultado da sessão; se houver uma captura ativa, ela é descartada.

Por padrão, se outro aplicativo estiver em foco ao concluir, o Hex não altera o clipboard
nem cola automaticamente. O aviso **Dictation ready** aparece por alguns
segundos, e **Paste Last Dictation (ready)** fica disponível no menu. Coloque
o cursor no destino e use essa ação para inserir o texto. O resultado fica
apenas na memória até outro ditado substituí-lo ou o app encerrar; entra no
History somente depois da colagem. A proteção identifica o aplicativo,
não a janela ou o campo dentro dele.

Em **Settings → Paste Last**, **Copy when auto-paste fails** pode manter a
transcrição no clipboard quando o Hex detecta erro de colagem ou mudança do
aplicativo de destino. A opção vem desligada. Quando ativada, o aviso
**Dictation copied** indica que você pode usar **⌘V**. Cancelamentos não
acionam a cópia. Falhas silenciosas do aplicativo receptor não são detectadas.

Os seletores de microfone, canal, idioma, modelo e retenção do History aceitam
teclado: **Tab** dá foco, **Enter** abre ou confirma, as **setas** percorrem as
opções e **Esc** fecha. A seleção do controle confirma o salvamento; mensagens
aparecem somente para erros ou resultados de testes explícitos. Durante uma operação com a chave, ações incompatíveis
ficam indisponíveis até ela terminar.

## Vocabulário personalizado

Em **Models → Keywords**, cada nome aparece como uma pílula. Digite a palavra
ou expressão e pressione **Return** para adicioná-la; espaços mantêm expressões
como “Claude Code” juntas. Colar várias linhas ou termos separados por vírgula
adiciona várias pílulas. O campo também conclui a inclusão ao perder o foco.

O **×** remove a pílula inteira. Com o campo vazio, **Backspace** remove a última;
**Esc** descarta apenas o texto ainda em edição. O mesmo controle aparece em
**Post-processing**, com a lista compartilhada. Não é necessário cadastrar
cada variação de um nome.

- **Send keywords** usa uma lista única para o principal e todos os fallbacks
  compatíveis. A seção aparece quando qualquer modelo da cadeia tem suporte.
  Cada adaptador envia somente os termos válidos que cabem nos seus limites.
  Os mesmos nomes ficam disponíveis para correção local em **Post-processing**.
- **Restore names locally** reconhece diferenças de caixa, espaços, hífens e
  pontuação de nomes completos. A grafia cadastrada prevalece sobre as outras
  opções de formatação.
- **Correct small spelling errors**, desligado por padrão, permite uma diferença
  de caractere em nomes longos, somente quando há um candidato claro. Não corrige
  automaticamente nomes curtos ou ambíguos, nem altera números/versões. O corretor evita URLs, caminhos,
  e-mails e trechos de código identificáveis no texto recebido.
- **Try the local correction** permite testar texto no próprio Mac, sem chamada
  ao modelo. A lista também faz parte da exportação/importação de preferências.

Cada ditado e Retry recebe uma cópia das regras vigentes. History e Paste Last
mantêm o resultado; editar o vocabulário não altera textos antigos.

### Como a compatibilidade é verificada

O catálogo do OpenRouter não informa de forma completa o suporte a vocabulário.
O Hex consulta as rotas e verifica os formatos conhecidos de Azure, OpenAI,
Groq e Deepgram. Usa um áudio sintético de aproximadamente dois segundos e nomes
fictícios: primeiro testa o parâmetro válido e um tipo inválido; quando necessário,
compara as grafias em uma sequência A/B/A. **HTTP 200 sozinho não comprova suporte.**
As verificações usam a chave existente e podem gerar pequenas cobranças de
transcrição do fornecedor. Nunca usam gravações ou nomes do usuário como amostra.

O resultado fica em cache por sete dias quando há evidência de suporte e por
um dia quando a verificação é inconclusiva. Falhas de acesso/rede expiram em
cinco minutos. **Recheck OpenRouter** repete a verificação. Vários endpoints para o
mesmo modelo não podem ser isolados nessa API; essas rotas e fornecedores sem
adaptador ficam com correção local. Aceitar um parâmetro ou influenciar uma
amostra não garante o reconhecimento de todo nome.

São aceitos até 2.000 nomes locais, com até 128 bytes por nome. Os limites de
envio dependem do modelo e do modo: nenhum termo é cortado no meio para caber.
No OpenRouter, Azure recebe até 2.000 nomes; os demais adaptadores usam limites
conservadores de até 50 nomes e 200–400 bytes. Os providers diretos usam seus
formatos próprios, incluindo keywords, keyterms e contexto de vocabulário.
Listas grandes podem aumentar a latência. No ElevenLabs, keyterms acrescentam
20% ao custo; mais de 100 termos em batch também impõem uma cobrança mínima
de 20 segundos. Essas regras pertencem ao provider e podem mudar.
Se um fornecedor rejeitar o vocabulário, o Hex tenta uma vez sem ele, respeitando
o prazo existente, e mantém a cadeia normal de fallbacks. A correção local continua.

Para verificar os modelos por linha de comando, usando a mesma amostra sintética:

```sh
cargo run -- validate-vocabulary
```

Referências: [OpenRouter STT](https://openrouter.ai/docs/guides/overview/multimodal/stt),
[Azure phrase lists](https://learn.microsoft.com/en-us/azure/ai-services/speech-service/improve-accuracy-phrase-list)
e [Deepgram keyterms](https://developers.deepgram.com/docs/keyterm).

## HUD

Em **HUD**, na barra lateral, escolha **Top** ou **Bottom** para posicionar o
indicador no topo ou na parte inferior da tela. **Display** escolhe a tela do
ponteiro, da janela ativa ou um monitor fixo. Se o monitor fixo desconectar,
o HUD acompanha o ponteiro até ele voltar. **Edge distance** ajusta a margem
entre 0 e 160 pontos; a posição respeita a área livre do Dock e da barra de menus.
O HUD e o aviso de ditado pronto acompanham o Space ativo do macOS, inclusive
ao trocar de Desktop ou entrar em tela cheia, sem tirar o foco do aplicativo.

**Size** oferece Small, Normal e Large. **Brightness** oferece Subtle, Normal
e Intense. Os padrões preservam o tamanho, o brilho e a animação existentes.

As cores de **Recording** e **Transcribing** são independentes. Cada uma tem
seis opções: vermelho, laranja, verde, turquesa, azul e roxo. O padrão continua
vermelho na gravação e azul na transcrição. A cápsula de preparação permanece
neutra e as animações são preservadas.

As escolhas são salvas automaticamente e aplicadas sem reiniciar o app. O
aviso de ditado pronto também acompanha a posição escolhida.

## Recuperar uma gravação

Quando a gravação termina, antes de entregar o resultado, o Hex salva uma cópia local do áudio
em WAV, com acesso restrito ao seu usuário. Se a conexão, a API ou todos os
fallbacks falharem, a gravação aparece em **History** com **Retry**, o aplicativo
em que o ditado começou e o motivo da falha (como timeout, conexão ou código
HTTP). Os detalhes exibidos não incluem respostas brutas ou credenciais. O áudio
salvo também pode ser recuperado depois de fechar e reabrir o aplicativo.

**Retry** usa a cadeia atual de **Models** e as chaves/opções de **Providers**. Só uma
recuperação manual roda por vez. Quando funcionar, o texto será salvo na
mesma entrada e poderá ser copiado com **Copy text**; não há colagem
automática. O áudio temporário só é removido depois que esse texto é salvo.

As entradas de recuperação não são apagadas pela retenção normal nem por
**Clear dictations**. Áudios com falha e textos recuperados ficam disponíveis
até você excluí-los na própria entrada, com confirmação. O diretório é
`~/Library/Application Support/hex-openrouter/recording-recovery/`.

Se o disco não permitir salvar, o Hex mantém uma cópia em memória durante a
sessão e mostra um aviso para manter o aplicativo aberto. Essa cópia não
sobrevive ao encerramento. No streaming, o envio pode começar antes desse salvamento. O recurso protege clipes concluídos; não é um gravador contínuo nem recupera uma captura descartada
com Esc ou áudio ainda em memória antes de começar a tentativa.

## Providers e Models

**Providers** guarda as chaves, as opções específicas de cada modelo e os
limites globais de tentativa. **Command + 8** abre a tela. **Models** define
um principal e até dois fallbacks, que podem pertencer a providers diferentes.
O provider aparece no nome do modelo; os indicadores de recursos mostram
streaming, keywords e contexto quando disponíveis.

| Provider | Integração |
| --- | --- |
| **OpenRouter** | Catálogo STT remoto, fallback e vocabulário por rotas verificadas. A API atual usada pelo Hex recebe clipes gravados. |
| **OpenAI** | GPT Transcribe e modelos GPT-4o/Whisper por upload; GPT Live Transcribe por WebSocket. Nomes são enviados como keywords ou prompt conforme o modelo. |
| **Deepgram** | Nova-3 e Nova-2, por upload ou streaming. Nova-3 aceita keyterms; formatação, pontuação e números têm controles próprios. |
| **ElevenLabs** | Scribe v2 por upload e Scribe v2 Realtime por WebSocket, com keyterms e opção de remover hesitações. |

Uma chave salva mostra somente os últimos quatro caracteres, com **Test** e
**Remove**. Clique no indicador da chave para substituí-la. O teste verifica acesso à conta; não garante saldo,
permissão para todos os modelos ou qualidade da transcrição.

Idioma, contexto, streaming e demais opções pertencem a cada combinação de
provider e modelo. Na primeira escolha, o modelo herda as opções compatíveis
do anterior. Ao voltar a um modelo já usado, suas escolhas são restauradas;
removê-lo da cadeia não apaga o perfil. As keywords são a exceção: uma lista
compartilhada por toda a cadeia, sem cópias por modelo.

**Streaming** envia áudio durante a gravação, mas o Hex só cola o texto final,
depois do pós-processamento. O toggle é explícito e começa desligado. Modelos
exclusivamente realtime precisam dele ligado; desligá-lo faz a cadeia pular
esse modelo. Não há troca silenciosa por outro modelo com nome parecido.
Os fallbacks começam após o modelo anterior falhar; o app não envia o áudio
a todos os providers ao mesmo tempo.

Streaming usa PCM contínuo e não compacta pausas como o **Trim silence** de
clipes gravados. A checagem final de silêncio evita colar uma resposta sem fala,
mas o áudio pode já ter sido transmitido e cobrado. Falhas de rede, perda de
blocos ou divergência na fronteira do atalho invalidam a resposta ao vivo. O
Hex tenta transcrever o clipe final completo, respeitando os limites da cadeia.
Nenhum texto parcial é colado.

As opções são salvas ao mudar o controle. Campos de texto salvam ao perder o
foco ou pressionar Return; **Esc** cancela a edição. Valores inválidos mantêm
a configuração anterior. A sessão de streaming em andamento mantém sua própria cópia das
opções e das keywords; mudanças valem para os próximos ditados.

Qualquer falha permite tentar o próximo modelo. Um HTTP 429 com espera dentro
do limite configurado recebe uma tentativa adicional no mesmo modelo. Clipes
longos enviados após a gravação são divididos em trechos, cada um com a cadeia
e seu limite de tempo. A URL customizada em Advanced pertence somente ao
OpenRouter e exige HTTPS, exceto em loopback local; os providers diretos usam os endereços oficiais.

### Configuração em arquivo

As opções continuam em `~/Library/Application Support/hex-openrouter/openrouter.json`
para preservar instalações anteriores. IDs sem prefixo explícito continuam
sendo do OpenRouter. Providers diretos usam `provider::modelo`:

```json
{
  "base_url": "https://openrouter.ai/api/v1",
  "transcription": {
    "models": ["deepgram::nova-3", "openai::gpt-transcribe", "microsoft/mai-transcribe-2"],
    "model_options": {
      "deepgram::nova-3": { "language": "pt", "streaming": true },
      "openai::gpt-transcribe": { "language": "pt" }
    },
    "trim_silence": true,
    "attempt_timeout_seconds": 30,
    "total_timeout_seconds": 90,
    "chunk_seconds": 120,
    "rate_limit_retry_max_wait_ms": 2000
  }
}
```

Prefira o Keychain. Variáveis `OPENROUTER_API_KEY`, `OPENAI_API_KEY`,
`DEEPGRAM_API_KEY` e `ELEVENLABS_API_KEY` têm precedência sobre a chave salva
correspondente. Apenas o OpenRouter preserva a compatibilidade com o antigo
campo `api_key` no arquivo. Chaves e endpoints não entram na exportação.

Contratos oficiais: [OpenAI file transcription](https://developers.openai.com/api/docs/guides/speech-to-text),
[OpenAI realtime](https://developers.openai.com/api/docs/guides/realtime-transcription),
[Deepgram live audio](https://developers.deepgram.com/reference/speech-to-text/listen-streaming),
[ElevenLabs Scribe](https://elevenlabs.io/docs/overview/capabilities/speech-to-text).

## Post-processing

O menu lateral **Post-processing** reúne regras locais para formatar o texto
antes de colar. Todas começam desligadas, não fazem outra chamada a modelo e
têm um exemplo de resultado na própria tela. **Command + 7** abre a seção.

| Opção | Efeito |
| --- | --- |
| **Lowercase text** | Converte todas as letras para minúsculas. |
| **Lowercase first letter** | Converte só a primeira letra, inclusive depois de aspas ou outros sinais. |
| **Remove punctuation** | Remove pontuação Unicode, mantendo palavras separadas; também afeta separadores de números e endereços. |
| **Remove ellipses** | Remove `…`, `...` e sequências equivalentes de pontos. |
| **Remove final period** | Retira o ponto final; preserva interrogações, exclamações e reticências. |
| **Collapse spaces** | Reduz espaços e tabulações repetidos, preservando parágrafos. |
| **Single line** | Une quebras de linha e parágrafos com espaços. |

Minúsculas para o texto inteiro já cobre a primeira letra; remover toda a
pontuação já cobre reticências e ponto final. As opções específicas ficam
indisponíveis enquanto a regra mais abrangente está ligada.

As regras são capturadas ao enviar o ditado. History, Paste Last e a cópia
após falha usam o resultado formatado; alterações posteriores não reescrevem
ditados anteriores. Um resultado vazio não cola nem envia Enter. Retry usa
as opções atuais; se nada restar após a formatação, mantém o áudio salvo.
As estatísticas continuam medindo a transcrição antes dessa formatação local.

## Settings

As preferências gerais do aplicativo ficam em **Settings**:

| Controle | Comportamento |
| --- | --- |
| **Dictation / Paste last** | Atalhos, Reset, modos Tap or hold/Hold only/Double tap, sensibilidade dos dois toques e envio opcional com Enter. |
| **Application / Sounds** | Sons, ícone no Dock e início no login. |
| **Preferences** | Importação e exportação das preferências. |

## Microphone

O menu lateral **Microphone** reúne as configurações de entrada e gravação,
incluindo o comportamento de outros áudios durante o ditado. Também pode ser
aberto pelo menu do app ou com **Command + 6**.

| Controle | Comportamento |
| --- | --- |
| **Input device / Automatic priority** | Dispositivo de entrada e ordem dos microfones preferidos. |
| **Microphone mode** | Mantém o dispositivo pronto ou o libera enquanto está ocioso. |
| **Input channel** | Mantém a mistura atual por padrão. Em interfaces com vários canais, permite escolher explicitamente o canal do microfone. A escolha fica vinculada ao dispositivo. |
| **Input levels** | Mostra RMS, pico e avisos de sinal muito baixo, ausência de sinal ou possível clipping na última gravação analisada. |
| **Trim silence** | Remove silêncio nas bordas e reduz pausas longas antes do envio. Usa uma heurística de energia. Atua antes do upload de clipes completos; streaming contínuo não compacta pausas. Pode ser desligado. |

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
**Pause** pausa e retoma os players; **Keep** mantém o áudio sem alterações.

**While dictating → Lower** reduz o áudio sem silenciar. O padrão
mantém **80% do volume anterior**: se estava em 50%, passa a 40% durante o
ditado. Em **Volume while dictating**, digite um percentual de 0 a 100 e
saia do campo para salvar automaticamente, ou pressione Return. A alteração vale para o próximo
ditado; ao terminar, o volume anterior volta com fade. Ajustes manuais de
volume continuam sendo preservados.

Em **Settings → Sounds**, início, término e erro/cancelamento têm volumes independentes,
incluindo Off para cada evento. A migração preserva o volume que você já usava,
inclusive sons desligados e o reforço anterior do sinal de início.

**Input priority** organiza os microfones usados por **Automatic**. Adicione
os dispositivos e use as setas para ordenar. Preferências explícitas e o
override de linha de comando continuam tendo precedência. A lista vazia mantém
a seleção automática anterior. Reconexões são verificadas em background e a
troca acontece entre ditados; liberar o microfone ocioso continua sem abri-lo.

Em **Preferences → Export / Import**, o Hex usa um arquivo JSON versionado para
os controles do app e as preferências de transcrição, incluindo modo de gravação,
envio com Enter, redução de volume e cópia após falha de colagem. Chave, endereço da API,
permissões, início no login, histórico, retenção e áudio não são exportados nem
alterados pela importação. O arquivo é validado antes da aplicação; falhas de
salvamento tentam restaurar as preferências anteriores. Arquivos maiores que
256 KiB ou de formato incompatível são recusados.

## History e Statistics

**History** registra os ditados colados com sucesso: texto, aplicativo em
foco, duração, latência, providers, modelos, fallbacks e corte de silêncio. Os detalhes de cada chamada mostram streaming durante a gravação ou envio posterior, quantidade de keywords enviadas e resultado da tentativa. Registros antigos não inventam esses metadados.
Tem busca, cópia e controles de retenção e limpeza. A retenção padrão é de
**7 dias**, com limites adicionais de quantidade e tamanho. A atualização da
versão 2.x preserva o texto e os relatórios de transcrição existentes.

**Statistics** reúne palavras, ditados, áudio gravado e enviado, clipes
silenciosos, tokens, custo informado pelo OpenRouter, latência de transcrição,
fallbacks e erros por tipo e modelo. Não armazena texto nem áudio. A latência
mostrada corresponde ao processamento da transcrição, sem o tempo na fila
ou na colagem. Em streaming, a latência mede a espera para concluir depois do fim da gravação. Um ditado com vários trechos pode contar para mais de um modelo.

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

O áudio vai ao provider escolhido. Ao usar OpenRouter, também passa por ele.
Em streaming, o envio começa durante a fala; o salvamento para recuperação ocorre
ao concluir a gravação, antes de entregar o resultado. Gravações com falha
ficam no Mac para recuperação, até Retry funcionar ou você excluí-las.
As regras de retenção desses serviços são independentes dos controles locais do app.

Os arquivos locais ficam em `~/Library/Application Support/hex-openrouter`:

| Local | Conteúdo |
| --- | --- |
| `settings.json` | Preferências do aplicativo e atalhos. |
| `openrouter.json` | Cadeia e perfis por provider/modelo; a chave legada do OpenRouter só aparece se configurada em texto puro. |
| `history.json` | Texto dos ditados e metadados, conforme a retenção escolhida. |
| `recording-recovery/` | WAVs de tentativas pendentes/com falha e textos recuperados; acesso restrito ao usuário. |
| `stats.json` | Totais diários, sem texto, áudio ou corpos de respostas de erro. |
| `logs/live.ndjson` e `logs/process.log` | Eventos e diagnósticos. O log de eventos inclui texto colado e aplicativo em foco; erros podem incluir detalhes retornados pelo provedor. |

Desligar ou limpar o **History** não desliga nem limpa os logs. Considere seu
conteúdo antes de compartilhá-los para diagnóstico. As chaves são acessadas diretamente pelo Security.framework e ficam somente
na memória dos transportes, sem aparecer nos argumentos de processos. O app
não altera automaticamente o acesso de itens antigos do Keychain. Os logs são
privados ao usuário e têm rotação ao iniciar; traces internos de HTTP/WebSocket
são bloqueados para não registrar headers, áudio ou URLs contendo keywords.

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
cargo run -- preview providers
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
