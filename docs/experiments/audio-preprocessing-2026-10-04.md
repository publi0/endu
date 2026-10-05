# Pré-processamento de áudio: piloto com MAI

Data: 4 de outubro de 2026. Status: experimento exploratório concluído.

**Recomendação: manter o processamento de referência e não habilitar redução
de ruído por padrão com base neste piloto.** Nenhum filtro adicional testado
melhorou o erro agregado do MAI no conjunto principal. Silero merece uma
avaliação como detector de presença de fala para evitar requisições sem fala.
A combinação de DeepFilter com o corte de silêncio por energia precisa ser
corrigida antes de qualquer proposta de integração: descartou fala válida em
volume baixo.

Os [resultados agregados em JSON](audio-preprocessing-2026-10-04.json) incluem
WER, CER, resultados por condição, tempos, custos e limitações. Este documento
registra as medições e as recomendações; a integração de novos filtros continua
sendo uma proposta.

## Amostra e protocolo

- Corpus público [Google FLEURS](https://huggingface.co/datasets/google/fleurs),
  configuração `pt_br`, divisão `validation`.
- Doze gravações, com 11 frases de referência distintas, 278 palavras de
  referência por condição e 146,94 segundos de áudio original. Todas têm
  rótulo masculino no corpus; não se presume que sejam 12 locutores distintos.
- Seleção: primeiras 12 gravações com duração entre 7 e 18 segundos nas
  primeiras 100 linhas disponíveis. Índices: `0, 1, 3, 4, 5, 6, 7, 8, 9, 10,
  11, 12`. Nenhum resultado de transcrição foi usado na seleção.
- Entrada mono a 16 kHz. Preparação comum: pico ajustado para 0,5 e adição de
  600 ms antes e 800 ms depois do áudio. Todas as variantes de uma condição
  receberam a mesma entrada.
- MAI: `microsoft/mai-transcribe-2` via OpenRouter, endpoint
  `/api/v1/audio/transcriptions`, idioma fixo `pt`, temperatura `0.0`, WAV
  PCM16 mono a 16 kHz. Requisições sequenciais, com ordem dos casos e dos
  métodos dentro de cada caso embaralhada com semente fixa no piloto principal.
- WER é a soma de substituições, exclusões e inserções, dividida pelo número
  de palavras de referência. A avaliação remove pontuação, converte para
  minúsculas e normaliza espaços. Não equipara números escritos por extenso
  a algarismos nem corrige diferenças semânticas. Menor é melhor.
- Um clipe com fala rejeitado pelo pré-processamento conta como transcrição
  vazia, incluindo todas as exclusões no WER. Não é retirado da comparação.

As quatro condições principais foram: sem ruído acrescentado; volume reduzido
em 24 dB; mistura sintética de ventilador, zumbido e cliques com relação
sinal/ruído de 10 dB; e zumbido sintético de 60/120 Hz também a 10 dB.
A mistura usa ruído determinístico por gravação. Quando necessário, toda a
mistura é reduzida proporcionalmente para manter o pico em até 0,95.

Foram 48 casos de fala por método: 12 gravações × 4 condições. Com oito
métodos, o piloto principal avaliou 384 combinações. O cenário de ruído mais
forte e o diagnóstico do DeepFilter foram executados depois e são apresentados
separadamente.

## Métodos comparados

| Método | Configuração |
| --- | --- |
| Corte do Hex | Snapshot do VAD por energia, compilado em um executável isolado. |
| Sem filtro/corte adicional | Mantém a entrada comum preparada para cada condição. |
| Passa-altas de 80 Hz | Butterworth de segunda ordem, seguido do corte do Hex. |
| Ganho limitado | Ganho fixo por gravação calculado pelo percentil 90 do RMS em janelas de 20 ms, limitado a +12 dB e ao pico disponível; seguido do corte do Hex. Não é AGC dinâmico. |
| Silero | Modelo ONNX, quadros de 512 amostras, limiares de 0,5/0,35, fala mínima de 160 ms, silêncio de 300 ms e margens de 200 ms; encurta intervalos internos longos. |
| RNNoise leve | Modelo original Xiph `std.rnnn` via FFmpeg `arnndn`, mistura de 50%, seguido do corte do Hex. |
| RNNoise completo | Mesmo modelo, mistura de 100%, seguido do corte do Hex. |
| DeepFilter + corte | Binário oficial `deep-filter` v0.5.6 para macOS arm64, modelo DeepFilterNet3, atenuação limitada a 12 dB e compensação de atraso, seguido do corte do Hex. |

RNNoise e DeepFilter receberam áudio reamostrado de 16 para 48 kHz, com retorno
a 16 kHz antes do corte e do envio. Isso não equivale a uma captura nativa em
48 kHz.

SHA-256 do arquivo de VAD usado como referência:

```text
6364f15e0a1e567edfaaec6e353e1f8533d41cb62d0f40e00aec053cacff7e58
```

O hash identifica o snapshot testado. Resultados deste documento não devem ser
atribuídos automaticamente a revisões posteriores do VAD.

## Resultado principal com MAI

Cada linha usa as mesmas 1.112 palavras de referência, somadas nas quatro
condições. Essa soma repete as frases entre condições; não representa 1.112
palavras de conteúdo independente.

| Método | Erros de palavras | WER |
| --- | ---: | ---: |
| Corte do Hex | 23 | **2,07%** |
| Passa-altas de 80 Hz + corte | 25 | 2,25% |
| Ganho limitado + corte | 25 | 2,25% |
| Silero | 25 | 2,25% |
| Sem filtro/corte adicional | 26 | 2,34% |
| RNNoise leve + corte | 30 | 2,70% |
| RNNoise completo + corte | 30 | 2,70% |
| DeepFilter + corte | 116 | 10,43% |

As diferenças entre os primeiros métodos são de poucas palavras e não
estabelecem uma classificação geral confiável. O JSON contém intervalos
exploratórios de bootstrap pareado, agrupando repetições pelo identificador da
frase de referência. A amostra pequena limita sua interpretação.

Como controle, 30 grupos de arquivos idênticos por SHA-256 foram enviados
por mais de uma variante. Nenhum desses grupos produziu textos normalizados
diferentes no MAI. Esse controle não elimina toda possível variação do serviço.

## Diagnóstico: DeepFilter e corte de silêncio

A regressão ficou concentrada no áudio com volume reduzido em 24 dB. Para
investigar a interação entre os componentes, as mesmas 12 saídas do DeepFilter
nessa condição foram enviadas novamente, desta vez sem o corte posterior.

| Pipeline no cenário de volume baixo | Erros / 278 palavras | WER | Gravações inteiras rejeitadas |
| --- | ---: | ---: | ---: |
| Corte do Hex | 6 | 2,16% | 0 |
| DeepFilter + corte do Hex | 98 | 35,25% | 2 |
| DeepFilter sem corte posterior | 7 | 2,52% | 0 |

A maior parte da piora observada desapareceu ao retirar o VAD por energia
depois do denoiser. Portanto, o resultado agregado de 10,43% não sustenta a
afirmação de que DeepFilter sozinho sempre prejudica a transcrição. Ele mostra
um problema concreto nessa combinação de etapas e parâmetros.

## Detecção de áudio sem fala

Foram preparados quatro exemplos sintéticos de dez segundos: silêncio,
ventilador, zumbido e cliques, todos sem fala.

| Detector | Exemplos sem fala que passariam para envio | Gravações inteiras de fala rejeitadas no piloto principal |
| --- | ---: | ---: |
| Corte do Hex | 3 de 4 | 0 de 48 |
| Silero | 0 de 4 | 0 de 48 |

Silero preservou algum áudio em todas as gravações de fala, mas seus cortes
alteraram a transcrição em alguns casos. O próximo experimento sugerido é usá-lo
somente para decidir se existe fala, mantendo o processamento de referência
quando houver. Essa integração ainda não foi testada.

As sete requisições efetivamente enviadas ao MAI com exemplos sem fala
— quatro sem corte e três após o corte do Hex — retornaram texto vazio.
Neste piloto, o benefício observado do detector neural seria evitar essas
requisições. Não houve evidência de redução de alucinações, pois nenhuma
transcrição não vazia ocorreu nesses exemplos.

## Teste complementar com ruído mais forte

As mesmas 12 gravações receberam a mistura sintética de escritório a 0 dB de
relação sinal/ruído: potências iguais de sinal e ruído antes do ajuste de pico.
Foram mais 96 combinações. Cada método tem 278 palavras de referência.

| Método | Erros de palavras | WER |
| --- | ---: | ---: |
| Sem filtro/corte adicional | 12 | 4,32% |
| Corte do Hex | 14 | 5,04% |
| Ganho limitado + corte | 14 | 5,04% |
| Passa-altas de 80 Hz + corte | 15 | 5,40% |
| RNNoise leve + corte | 16 | 5,76% |
| Silero | 17 | 6,12% |
| RNNoise completo + corte | 20 | 7,19% |
| DeepFilter + corte | 22 | 7,91% |

Nenhum filtro adicional superou o corte de referência. Manter o áudio sem
corte teve dois erros a menos, uma diferença insuficiente para recomendar
desligar o corte de silêncio em geral.

## Verificação com outro transcritor

O mesmo conjunto principal foi avaliado localmente com
[`mlx-community/whisper-small-mlx`](https://huggingface.co/mlx-community/whisper-small-mlx),
revisão `45f3915923c7a79a5a5b5a7d909d39aeb0e5630e`, idioma português e
temperatura zero. Os WERs foram: sem corte, 7,91%; corte do Hex, 8,72%; ganho
limitado, 8,27%; passa-altas, 9,17%; Silero, 9,62%; RNNoise completo, 9,71%;
RNNoise leve, 10,25%; DeepFilter + corte, 17,90%.

A ordem dos resultados mudou entre transcritores. Uma melhoria em um modelo
não deve ser presumida para MAI, GPT Transcribe ou Nova.

## Tempo e custo

Medianas de pré-processamento por clipe no conjunto principal: corte do Hex,
11,9 ms; ganho limitado, 13,6 ms; passa-altas, 15,0 ms; Silero, 46,5 ms;
RNNoise, aproximadamente 160–162 ms; DeepFilter, 647,9 ms.

Essas medições vêm de um processamento em lote com executáveis auxiliares e
modelos carregados conforme cada implementação. Incluem abertura de processos
e inicialização quando presentes; não representam latência incremental dentro
do app nem uma comparação entre implementações igualmente otimizadas.

| Etapa com MAI | Combinações registradas | Chamadas à API |
| --- | ---: | ---: |
| Piloto principal | 384 | 382 |
| Diagnóstico do DeepFilter e exemplos sem fala | 20 | 19 |
| Ruído mais forte | 96 | 96 |
| **Total** | **500** | **497** |

As três combinações restantes foram rejeitadas localmente pelo VAD. Todas as
497 chamadas retornaram HTTP 200 e custo informado. O custo total retornado
pela API foi **US$ 0,16130556**, dentro do limite de US$ 2 definido para o
experimento. Os testes locais com Whisper não usam a API.

## Limites e próximos testes

- Amostra pequena de fala lida, com rótulos masculinos e 11 frases distintas.
  Não avalia adequadamente outras vozes, fala espontânea ou variedade de sotaques.
- Foram usados áudio público e ruído sintético. Não houve captura pelo
  microfone do usuário, eco real de sala ou vozes sobrepostas.
- A normalização inicial de pico também limita a transferência dos resultados
  para níveis de entrada reais. A redução de 24 dB foi uma condição controlada.
- O idioma foi fixado em português; o modo Auto-detect não foi avaliado.
- O RNNoise testado usa o modelo original `std.rnnn`. O resultado não representa
  todos os modelos ou integrações RNNoise disponíveis.
- Apple Voice Processing, cancelamento de eco com referência de reprodução e
  AGC dinâmico não foram testados. Exigem uma avaliação própria de captura.
- Antes de mudar padrões, repetir uma comparação pareada com gravações
  representativas dos microfones e ambientes reais. Medir WER, rejeições de
  fala válida, envios sem fala e latência na integração efetiva.

Os resultados agregados estão preservados no JSON ao lado deste documento.
Os scripts, áudios públicos transformados e saídas por requisição permaneceram
no laboratório local; não fazem parte deste registro no repositório. O JSON
permite conferir as tabelas, mas não é um pacote completo para reproduzir os
experimentos sozinho.

## Referências dos componentes

- [Silero VAD](https://github.com/snakers4/silero-vad).
- [RNNoise](https://github.com/xiph/rnnoise) e
  [modelos para o filtro arnndn](https://github.com/richardpl/arnndn-models).
- [DeepFilterNet](https://github.com/Rikorose/DeepFilterNet) e
  [binários v0.5.6](https://github.com/Rikorose/DeepFilterNet/releases/tag/v0.5.6).
- [Google FLEURS](https://huggingface.co/datasets/google/fleurs).
