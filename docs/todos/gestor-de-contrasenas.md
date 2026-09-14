# Gestor de contraseñas

Estado: retirado del código, pendiente de rehacer entero. La contraseña por
defecto se quitó porque era la cuarta parte de una función, y la cuarta parte
que menos paga.

- `04b4fc1` — `feat(arca-gui): offer the default password wherever one is asked for`, la versión que se retira.

## Por qué existe este documento

Arca tenía una «contraseña por defecto»: se ponía con `Ctrl+P`, vivía en
memoria hasta cerrar la ventana, y aparecía como un botón *Usar la contraseña
por defecto* en los cuatro sitios donde se pide una — abrir, extraer, cambiar
la contraseña de un archivo y elegir la de uno nuevo.

El botón no desbloqueaba nada. Solo escribía el texto en el campo:

```rust
this.controller.state.password_input = kept.clone();
```

Eso era deliberado y el razonamiento era bueno para **poner** una contraseña
—una puesta por error cuesta reescribir el archivo entero para quitarla— pero
se aplicaba igual a **abrir**, donde no hay nada que reescribir y un intento
fallido no cuesta nada.

El resultado era una función que ahorraba teclear una clave larga a cambio de
dos clics, solo dentro de una sesión, y que había que activar antes con
`Ctrl+P`. Si abres un archivo cifrado al día, cuesta más de lo que ahorra.
Solo empieza a pagar con una carpeta entera de archivos con la misma clave, y
justo ese caso es el que peor cubría, porque seguía pidiendo dos clics por
archivo.

Además el comentario que la describía prometía algo que el código no hacía
(«One password **to try before asking**» — nunca se intentaba sola) y apuntaba
a una función, `default_password_window`, que no existía en el árbol.

## Lo que hace la competencia

Comprobado, no supuesto.

| | ¿Tiene? | Cómo |
| --- | --- | --- |
| 7-Zip | No | No hay contraseña por defecto configurable. Sí cachea en memoria sola durante la sesión, y eso genera la queja contraria: hay un hilo en su foro titulado *«How to stop caching passwords»* |
| NanaZip | No | Hereda el comportamiento de 7-Zip. Petición abierta [M2Team/NanaZip#321](https://github.com/M2Team/NanaZip/issues/321), *«Add password manager with auto-try fill»*. Existe un fork entero, `SanRive/NanaZipPasswordManager`, creado solo para añadirlo |
| WinRAR | Sí | Diálogo *Organize passwords*, con registros etiquetados y persistentes |

La demanda es real: a la competencia se lo piden, y en el caso de NanaZip
alguien bifurcó el proyecto para tenerlo. La conclusión no es que la idea
fuese mala, sino que media implementación no sirve.

## Lo que haría falta para rehacerlo

WinRAR ya resolvió el diseño y conviene copiarle las cuatro piezas, porque
cada una responde a una pregunta que la versión retirada dejaba abierta.

1. **Máscara por archivo** (`Select for archives`). Una contraseña se asocia a
   un patrón, no a «la sesión». Es lo que hace que la carpeta de archivos con
   la misma clave funcione, que es el único caso que justifica la función.
2. **Casilla «aceptar sin confirmación»** por registro. Es el intento
   automático, pero como opción y por contraseña, no como comportamiento fijo.
   La advertencia de 7-Zip va justo aquí: cachear sin preguntar molesta, así
   que la prudencia de la versión retirada —ofrecer, no aplicar— debe seguir
   siendo lo predeterminado.
3. **Contraseña maestra**. Es el precio de persistir entre sesiones. Sin ella
   no se guarda nada: una contraseña en claro en `gui.conf`, al lado del tema y
   los anchos de columna, es cómo un archivo cifrado deja de estar cifrado.
4. **Etiquetas**, para poder mirar la lista y saber qué es cada entrada sin
   enseñar el texto de la contraseña.

## La parte de seguridad, que no es opcional

Hoy ninguna contraseña toca el disco. `Settings` —lo único que se escribe— no
tiene ningún campo donde meterla, y todas viven en `AppState`, en memoria, y
mueren con el proceso. Cualquier versión futura tiene que mantener esa
propiedad salvo que llegue con la contraseña maestra del punto 3.

Aparte, y ya en el árbol actual: no hay `zeroize` ni `secrecy` en ningún
`Cargo.toml`. Las contraseñas son `String` normales, así que al liberarse no se
sobrescriben y pueden acabar en `pagefile.sys` o en un volcado de fallo. Eso
protege de «alguien lee mi fichero de configuración», que era el objetivo, pero
no de un análisis forense de memoria. Un gestor que persista sube bastante lo
que hay en juego, así que `Zeroizing<String>` debería entrar en el mismo
trabajo, con cuidado en los clones que se reparten a los hilos.

## Qué se quitó exactamente

Para poder reconstruirlo sin arqueología:

- `AppState::default_password` y `AppState::asking_default_password`.
- `ModalKind::DefaultPassword` y su diálogo.
- `OverflowAction::DefaultPassword` y su entrada de menú.
- `Shortcut::DefaultPassword` y el atajo `Ctrl+P`.
- `keep_default_password` y `forget_default_password`.
- Los dos botones *Usar la contraseña por defecto*: el del diálogo que pide
  contraseña y el de la caja de comprimir.
- Las cadenas `default_password`, `use_default_password`, `password_kept` y
  `password_forgotten`. `password_hint` se queda, porque la usa también el
  diálogo normal.
